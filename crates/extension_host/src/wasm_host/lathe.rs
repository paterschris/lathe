//! Host side of the Lathe-only extension API (`lathe:extension`).
//!
//! A Lathe extension is one wasm component that implements both a
//! `zed:extension` world and the `lathe:extension` world. It is linked against
//! the matching Zed bindings plus the Lathe imports below, and instantiated
//! once; both sets of typed bindings are then loaded from that one instance.

use super::wit::{since_v0_6_0, since_v0_8_0};
use super::{WasmState, wit::new_linker};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::BTreeSet;
use futures::FutureExt as _;
use gpui::AsyncApp;
use gpui::BackgroundExecutor;
use release_channel::ReleaseChannel;
use semver::Version;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use wasmtime::Store;
use wasmtime::component::{Component, Linker};

/// Lathe API versions this host implements. Extensions embed the version of
/// `lathe_extension_api` they were built with; any patch release of a
/// supported minor is accepted, because patch releases of the crate may only
/// change guest-side code, never the WIT.
pub const API_VERSION_V0_1: Version = Version::new(0, 1, 0);
/// Adds views (`lathe:extension/views@0.2.0`) on top of everything in 0.1.
pub const API_VERSION_V0_2: Version = Version::new(0, 2, 0);

/// The custom section the `lathe_extension_api` crate embeds in an extension.
pub const API_VERSION_SECTION: &str = "lathe:api-version";

wasmtime::component::bindgen!({
    imports: {
        default: async | trappable,
    },
    exports: {
        default: async,
    },
    path: "../lathe_extension_api/wit/since_v0.1.0",
    world: "lathe:extension/extension@0.1.0",
});

/// Bindings for the 0.2 world. It includes the 0.1 world unchanged, so every
/// 0.1 interface maps to the 0.1 bindings above and keeps its single host
/// implementation; only interfaces new in 0.2 need host code here.
pub mod v0_2_0 {
    wasmtime::component::bindgen!({
        imports: {
            default: async | trappable,
        },
        exports: {
            default: async,
        },
        path: "../lathe_extension_api/wit/since_v0.2.0",
        world: "lathe:extension/extension@0.2.0",
        with: {
            "lathe:extension/commands@0.1.0": super::lathe::extension::commands,
            "lathe:extension/buffers@0.1.0": super::lathe::extension::buffers,
            "lathe:extension/ui@0.1.0": super::lathe::extension::ui,
            "lathe:extension/events@0.1.0": super::lathe::extension::events,
            "lathe:extension/tasks@0.1.0": super::lathe::extension::tasks,
            "lathe:extension/extension-settings@0.1.0": super::lathe::extension::extension_settings,
        },
    });
}

/// The Lathe exports of a loaded extension, by the API version it was built with.
pub(super) enum LatheBindings {
    V0_1(Extension),
    V0_2(v0_2_0::Extension),
}

pub use self::lathe::extension::buffers::{
    BufferEdit, BufferSnapshot as WitBufferSnapshot, ByteRange,
};
pub use self::lathe::extension::commands::CommandSpec;
pub use self::lathe::extension::events::BufferEvent as WitBufferEvent;
pub use self::lathe::extension::tasks::{TaskReveal, TaskSpec};
pub use self::lathe::extension::ui::{NotificationLevel, PickItem};

/// A permission an extension declares in `lathe_capabilities` in its
/// `extension.toml`. Lathe API calls that need an undeclared capability fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LatheCapability {
    /// Read the text, path, language and selections of the active buffer.
    BufferRead,
    /// Edit the active buffer and change its selections.
    BufferWrite,
    /// Receive an event when a file is opened.
    EventsOpen,
    /// Receive an event when a file is saved.
    EventsSave,
    /// Receive an event when a file's text changes.
    EventsChange,
    /// Set status bar text and show notifications.
    UiStatus,
    /// Ask the user for input from a command.
    UiPrompt,
    /// Run commands as tasks in a terminal.
    TasksSpawn,
    /// Show views the extension describes (API 0.2).
    UiViews,
    /// Highlight text in the editor (API 0.2).
    UiDecorations,
}

impl LatheCapability {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "buffer:read" => Ok(Self::BufferRead),
            "buffer:write" => Ok(Self::BufferWrite),
            "events:open" => Ok(Self::EventsOpen),
            "events:save" => Ok(Self::EventsSave),
            "events:change" => Ok(Self::EventsChange),
            "ui:status" => Ok(Self::UiStatus),
            "ui:prompt" => Ok(Self::UiPrompt),
            "tasks:spawn" => Ok(Self::TasksSpawn),
            "ui:views" => Ok(Self::UiViews),
            "ui:decorations" => Ok(Self::UiDecorations),
            other => bail!("unknown Lathe capability {other:?}"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::BufferRead => "buffer:read",
            Self::BufferWrite => "buffer:write",
            Self::EventsOpen => "events:open",
            Self::EventsSave => "events:save",
            Self::EventsChange => "events:change",
            Self::UiStatus => "ui:status",
            Self::UiPrompt => "ui:prompt",
            Self::TasksSpawn => "tasks:spawn",
            Self::UiViews => "ui:views",
            Self::UiDecorations => "ui:decorations",
        }
    }

    /// A short description for the Extensions page.
    pub fn label(self) -> &'static str {
        match self {
            Self::BufferRead => "Reads files",
            Self::BufferWrite => "Edits files",
            Self::EventsOpen => "Runs when files open",
            Self::EventsSave => "Runs on save",
            Self::EventsChange => "Runs as you type",
            Self::UiStatus => "Shows status",
            Self::UiPrompt => "Asks for input",
            Self::TasksSpawn => "Runs commands",
            Self::UiViews => "Shows views",
            Self::UiDecorations => "Highlights text",
        }
    }

    pub fn parse_all(names: &[Arc<str>]) -> Result<BTreeSet<Self>> {
        names.iter().map(|name| Self::parse(name)).collect()
    }
}

/// Which declared Lathe capabilities the user has allowed, per extension.
/// Persisted as `lathe-grants.json` in the extensions directory. An extension
/// only gets the capabilities it declares *and* the user has granted.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct LatheGrants {
    #[serde(default)]
    pub extensions: std::collections::BTreeMap<Arc<str>, BTreeSet<String>>,
}

impl LatheGrants {
    pub fn granted(&self, extension_id: &str) -> BTreeSet<LatheCapability> {
        self.extensions
            .get(extension_id)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| LatheCapability::parse(name).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn grant(
        &mut self,
        extension_id: Arc<str>,
        capabilities: impl IntoIterator<Item = LatheCapability>,
    ) {
        let entry = self.extensions.entry(extension_id).or_default();
        entry.extend(
            capabilities
                .into_iter()
                .map(|capability| capability.name().to_string()),
        );
    }

    pub fn revoke(&mut self, extension_id: &str) {
        self.extensions.remove(extension_id);
    }
}

/// Lets the host stop a running guest call. The store checks it at every
/// epoch tick (about every 100 ms); once set, the next check traps the call,
/// which leaves the instance unusable, so the extension must be reloaded.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle(Arc<std::sync::atomic::AtomicBool>);

impl InterruptHandle {
    pub fn interrupt(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn take(&self) -> bool {
        self.0.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

/// A buffer event, as delivered to extensions that declared the matching
/// capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LatheBufferEvent {
    Opened,
    Saved,
    Changed,
}

impl LatheBufferEvent {
    pub fn capability(self) -> LatheCapability {
        match self {
            Self::Opened => LatheCapability::EventsOpen,
            Self::Saved => LatheCapability::EventsSave,
            Self::Changed => LatheCapability::EventsChange,
        }
    }

    fn to_wit(self) -> WitBufferEvent {
        match self {
            Self::Opened => WitBufferEvent::Opened,
            Self::Saved => WitBufferEvent::Saved,
            Self::Changed => WitBufferEvent::Changed,
        }
    }
}

/// Shows the Lathe API's UI. Implemented by the UI layer and installed with
/// [`super::WasmHost::set_lathe_ui`].
pub trait LatheUiDelegate: Send + Sync + 'static {
    fn set_status_text(
        &self,
        extension_id: Arc<str>,
        text: Option<String>,
        tooltip: Option<String>,
        cx: &mut AsyncApp,
    );
    fn show_notification(
        &self,
        extension_name: Arc<str>,
        level: NotificationLevel,
        message: String,
        cx: &mut AsyncApp,
    );
    /// Opens a text prompt; the receiver yields `None` if it is dismissed.
    fn prompt_input(
        &self,
        extension_name: Arc<str>,
        label: String,
        placeholder: Option<String>,
        cx: &mut AsyncApp,
    ) -> Result<futures::channel::oneshot::Receiver<Option<String>>>;
    /// Starts a task in a terminal. `task.label` is already prefixed with the
    /// extension's name.
    fn spawn_task(&self, task: TaskSpec, cx: &mut AsyncApp) -> Result<()>;
    /// Stores or replaces one of an extension's views, already validated.
    fn update_view(
        &self,
        extension_id: Arc<str>,
        view_id: Arc<str>,
        view: super::lathe_views::LatheView,
        cx: &mut AsyncApp,
    );
    fn remove_view(&self, extension_id: Arc<str>, view_id: Arc<str>, cx: &mut AsyncApp);
    /// Shows, replaces or (with `None`) hides an extension's status item. It
    /// shares the extension's status bar slot with `set_status_text`.
    fn set_status_item(
        &self,
        extension_id: Arc<str>,
        item: Option<LatheStatusItem>,
        cx: &mut AsyncApp,
    );
    /// Opens a file in the focused window and moves the cursor if a row is set.
    fn open_location(&self, location: LatheLocation, cx: &mut AsyncApp) -> Result<()>;
    /// Opens a picker; the receiver yields the chosen item's id.
    fn quick_pick(
        &self,
        extension_name: Arc<str>,
        label: String,
        items: Vec<PickItem>,
        cx: &mut AsyncApp,
    ) -> Result<futures::channel::oneshot::Receiver<Option<String>>>;
}

/// What a command sees of the buffer it was run from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBufferSnapshot {
    /// Changes whenever the buffer or its selections change.
    pub version: u64,
    pub path: Option<String>,
    pub abs_path: Option<String>,
    pub worktree_root: Option<String>,
    pub language: Option<String>,
    pub text: String,
    pub selections: Vec<Range<usize>>,
}

/// Gives a running command access to the editor it was run from. Implemented
/// by the UI layer, which knows which editor was focused.
pub trait LatheEditorDelegate: Send + Sync + 'static {
    /// Returns `Ok(None)` when the editor is not showing a single buffer.
    fn snapshot(&self, cx: &mut AsyncApp) -> Result<Option<ActiveBufferSnapshot>>;
    fn edit(
        &self,
        version: u64,
        edits: Vec<(Range<usize>, String)>,
        cx: &mut AsyncApp,
    ) -> Result<()>;
    fn select(&self, version: u64, ranges: Vec<Range<usize>>, cx: &mut AsyncApp) -> Result<()>;
    /// Replaces one extension's decorations in `layer` for this buffer.
    /// Rejects a stale `version` and ranges outside the text.
    fn set_decorations(
        &self,
        _version: u64,
        _extension_id: Arc<str>,
        _layer: Arc<str>,
        _decorations: Vec<LatheDecoration>,
        _cx: &mut AsyncApp,
    ) -> Result<()> {
        bail!("decorations are not available here")
    }
    /// Where the buffer's file sits in its project, so settings can be read
    /// for it, including a trusted project's own settings file.
    fn settings_location(
        &self,
        _cx: &mut AsyncApp,
    ) -> Option<(settings::WorktreeId, Arc<util::rel_path::RelPath>)> {
        None
    }
}

/// Checks that `ranges` lie within `text`, on character boundaries, and, when
/// `disjoint` is set, that they are sorted and do not overlap.
pub fn validate_ranges<'a>(
    text: &str,
    ranges: impl IntoIterator<Item = &'a Range<usize>>,
    disjoint: bool,
) -> Result<()> {
    let mut previous_end = 0;
    for (index, range) in ranges.into_iter().enumerate() {
        if range.start > range.end {
            bail!("range {range:?} starts after it ends");
        }
        if range.end > text.len() {
            bail!(
                "range {range:?} is past the end of the buffer ({} bytes)",
                text.len()
            );
        }
        if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            bail!("range {range:?} is not on a character boundary");
        }
        if disjoint {
            if index > 0 && range.start < previous_end {
                bail!("ranges must be sorted and must not overlap, got {range:?}");
            }
            previous_end = range.end;
        }
    }
    Ok(())
}

/// A command registered by an extension while it initialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatheCommand {
    pub id: Arc<str>,
    pub title: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatheDockSide {
    Left,
    Right,
    Bottom,
}

/// A panel registered by an extension while it initialized. It shows the
/// extension's view whose id is the panel's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LathePanel {
    pub id: Arc<str>,
    pub title: Arc<str>,
    pub icon: Option<Arc<str>>,
    pub dock: LatheDockSide,
}

/// A preview registered by an extension while it initialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LathePreview {
    pub id: Arc<str>,
    pub title: Arc<str>,
    pub file_suffixes: Arc<[Arc<str>]>,
}

impl LathePreview {
    pub fn matches(&self, file_name: &str) -> bool {
        self.file_suffixes
            .iter()
            .any(|suffix| file_name.ends_with(suffix.as_ref()))
    }
}

/// An extension's clickable status bar item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatheStatusItem {
    pub label: Arc<str>,
    pub icon: Option<Arc<str>>,
    pub tooltip: Option<Arc<str>>,
    pub tone: super::lathe_views::Tone,
}

/// Where a file to open is, as asked for by `open-location`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatheLocation {
    /// Absolute, or relative to a folder open in the window.
    pub path: String,
    /// 0-based, converted from the API's 1-based numbers.
    pub row: Option<u32>,
    pub column: Option<u32>,
}

/// Changes to where an extension's UI appears, sent to the extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatheSurfaceEvent {
    ViewShown(Arc<str>),
    ViewHidden(Arc<str>),
    StatusItemClicked,
    PreviewOpened {
        preview_id: Arc<str>,
        view_id: Arc<str>,
        path: Option<String>,
        abs_path: Option<String>,
    },
    PreviewChanged(Arc<str>),
    PreviewClosed(Arc<str>),
}

impl LatheSurfaceEvent {
    /// Clicks are something the user did, so the extension may open files
    /// and prompts while handling them.
    fn is_user_action(&self) -> bool {
        matches!(self, Self::StatusItemClicked)
    }

    /// The capability an extension needs to receive this event.
    pub fn capability(&self) -> LatheCapability {
        match self {
            Self::StatusItemClicked => LatheCapability::UiStatus,
            _ => LatheCapability::UiViews,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LatheDecorationStyle {
    Background,
    Underline,
    WavyUnderline,
    Gutter,
}

/// One decoration, with byte offsets into the buffer text it was set against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatheDecoration {
    pub range: Range<usize>,
    pub style: LatheDecorationStyle,
    pub tone: super::lathe_views::Tone,
    pub message: Option<Arc<str>>,
}

/// Most decorations in one layer.
pub const MAX_DECORATIONS_PER_LAYER: usize = 5_000;
/// Most layers one extension may have on one buffer.
pub const MAX_DECORATION_LAYERS: usize = 8;
/// Longest decoration message, in characters.
pub const MAX_DECORATION_MESSAGE_CHARS: usize = 200;

/// Most panels one extension may register.
pub const MAX_PANELS_PER_EXTENSION: usize = 4;
/// Most previews one extension may register.
pub const MAX_PREVIEWS_PER_EXTENSION: usize = 8;
/// Most file suffixes one preview may match.
pub const MAX_PREVIEW_SUFFIXES: usize = 16;
const MAX_TITLE_CHARS: usize = 100;
const MAX_SUFFIX_CHARS: usize = 64;

/// Whether this build accepts extensions that use the given Lathe API version.
pub fn is_supported_api_version(release_channel: ReleaseChannel, version: &Version) -> bool {
    [API_VERSION_V0_1, API_VERSION_V0_2]
        .iter()
        .any(|supported| version.major == supported.major && version.minor == supported.minor)
        && is_api_enabled(release_channel)
}

/// The Lathe API ships on Beta first, then Preview and Stable once it is frozen.
fn is_api_enabled(release_channel: ReleaseChannel) -> bool {
    match release_channel {
        ReleaseChannel::Dev | ReleaseChannel::Nightly | ReleaseChannel::Beta => true,
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            cfg!(any(test, feature = "test-support"))
        }
    }
}

/// Instantiates a component that implements the Lathe world on top of the Zed
/// world identified by `zed_api_version`, storing the Lathe bindings in the
/// store and returning the Zed bindings.
pub(super) async fn instantiate(
    executor: &BackgroundExecutor,
    store: &mut Store<WasmState>,
    release_channel: ReleaseChannel,
    zed_api_version: &Version,
    lathe_api_version: &Version,
    component: &Component,
) -> Result<super::wit::Extension> {
    if !is_supported_api_version(release_channel, lathe_api_version) {
        bail!(
            "this build of Lathe does not support version {lathe_api_version} of the Lathe extension API"
        );
    }

    let extension = if *zed_api_version >= since_v0_8_0::MIN_VERSION {
        super::wit::authorize_access_to_unreleased_wasm_api_version(release_channel)?;
        let instance = linker_v0_8_0(executor)
            .instantiate_async(&mut *store, component)
            .await
            .map_err(anyhow::Error::from)
            .context("failed to instantiate Lathe extension")?;
        let extension =
            since_v0_8_0::Extension::new(&mut *store, &instance).map_err(anyhow::Error::from)?;
        store.data_mut().lathe.bindings = Some(load_bindings(store, &instance, lathe_api_version)?);
        super::wit::Extension::V0_8_0(extension)
    } else if *zed_api_version >= since_v0_6_0::MIN_VERSION {
        let instance = linker_v0_6_0(executor)
            .instantiate_async(&mut *store, component)
            .await
            .map_err(anyhow::Error::from)
            .context("failed to instantiate Lathe extension")?;
        let extension =
            since_v0_6_0::Extension::new(&mut *store, &instance).map_err(anyhow::Error::from)?;
        store.data_mut().lathe.bindings = Some(load_bindings(store, &instance, lathe_api_version)?);
        super::wit::Extension::V0_6_0(extension)
    } else {
        bail!(
            "Lathe extensions must be built with zed_extension_api 0.6.0 or newer, \
             this one uses {zed_api_version}"
        );
    };

    Ok(extension)
}

fn load_bindings(
    store: &mut Store<WasmState>,
    instance: &wasmtime::component::Instance,
    lathe_api_version: &Version,
) -> Result<LatheBindings> {
    let bindings = if lathe_api_version.minor >= API_VERSION_V0_2.minor {
        v0_2_0::Extension::new(&mut *store, instance).map(LatheBindings::V0_2)
    } else {
        Extension::new(&mut *store, instance).map(LatheBindings::V0_1)
    };
    bindings.map_err(anyhow::Error::from).with_context(|| {
        format!("extension declares Lathe API {lathe_api_version} but does not implement it")
    })
}

/// The 0.2 world includes every 0.1 import, so one linker serves extensions
/// built against either version; a 0.1 extension simply doesn't import the
/// 0.2 interfaces.
fn add_lathe_imports(linker: &mut Linker<WasmState>) -> wasmtime::Result<()> {
    v0_2_0::Extension::add_to_linker::<_, WasmState>(linker, |state| state)
}

fn linker_v0_8_0(executor: &BackgroundExecutor) -> &'static Linker<WasmState> {
    static LINKER: OnceLock<Linker<WasmState>> = OnceLock::new();
    LINKER.get_or_init(|| {
        new_linker(executor, |linker| {
            since_v0_8_0::Extension::add_to_linker::<_, WasmState>(linker, |state| state)?;
            add_lathe_imports(linker)
        })
    })
}

fn linker_v0_6_0(executor: &BackgroundExecutor) -> &'static Linker<WasmState> {
    static LINKER: OnceLock<Linker<WasmState>> = OnceLock::new();
    LINKER.get_or_init(|| {
        new_linker(executor, |linker| {
            since_v0_6_0::Extension::add_to_linker::<_, WasmState>(linker, |state| state)?;
            add_lathe_imports(linker)
        })
    })
}

/// Per-extension Lathe state kept in the wasm store.
#[derive(Default)]
pub struct LatheState {
    pub(super) bindings: Option<LatheBindings>,
    /// Ids of the views this extension has created, to cap how many it keeps.
    pub(super) view_ids: collections::HashSet<Arc<str>>,
    /// Commands may only be registered while the extension initializes.
    pub(super) initializing: bool,
    pub(super) commands: Vec<LatheCommand>,
    pub(super) panels: Vec<LathePanel>,
    pub(super) previews: Vec<LathePreview>,
    pub(super) capabilities: BTreeSet<LatheCapability>,
    /// The editor the currently running command was invoked from, or the
    /// buffer the currently running event handler is about.
    pub(super) editor: Option<Arc<dyn LatheEditorDelegate>>,
    /// Prompts are only allowed while a command runs, so an event can never
    /// open a dialog on its own.
    pub(super) running_command: bool,
}

impl LatheState {
    fn require(&self, capability: LatheCapability) -> Result<()> {
        if !self.capabilities.contains(&capability) {
            bail!(
                "this extension did not declare the {:?} capability; add it to \
                 lathe_capabilities in extension.toml",
                capability.name()
            );
        }
        Ok(())
    }

    fn editor(&self) -> Option<Arc<dyn LatheEditorDelegate>> {
        self.editor.clone()
    }
}

pub(super) fn is_valid_command_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

impl lathe::extension::commands::Host for WasmState {
    async fn register_command(
        &mut self,
        spec: CommandSpec,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(register_command(&mut self.lathe, spec).map_err(|error| error.to_string()))
    }
}

impl lathe::extension::buffers::Host for WasmState {
    async fn active_buffer(
        &mut self,
    ) -> wasmtime::Result<Result<Option<WitBufferSnapshot>, String>> {
        if let Err(error) = self.lathe.require(LatheCapability::BufferRead) {
            return Ok(Err(error.to_string()));
        }
        let Some(editor) = self.lathe.editor() else {
            return Ok(Ok(None));
        };
        let snapshot = self
            .on_main_thread(move |cx| async move { editor.snapshot(cx) }.boxed_local())
            .await;
        Ok(snapshot
            .map(|snapshot| {
                snapshot.map(|snapshot| WitBufferSnapshot {
                    version: snapshot.version,
                    path: snapshot.path,
                    abs_path: snapshot.abs_path,
                    worktree_root: snapshot.worktree_root,
                    language: snapshot.language,
                    text: snapshot.text,
                    selections: snapshot.selections.into_iter().map(to_byte_range).collect(),
                })
            })
            .map_err(|error| format!("{error:#}")))
    }

    async fn edit_active_buffer(
        &mut self,
        version: u64,
        edits: Vec<BufferEdit>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::BufferWrite) {
            return Ok(Err(error.to_string()));
        }
        let Some(editor) = self.lathe.editor() else {
            return Ok(Err("this command was not run from an editor".into()));
        };
        let edits = match edits
            .into_iter()
            .map(|edit| Ok((from_byte_range(edit.range)?, edit.text)))
            .collect::<Result<Vec<_>>>()
        {
            Ok(edits) => edits,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let result = self
            .on_main_thread(move |cx| async move { editor.edit(version, edits, cx) }.boxed_local())
            .await;
        Ok(result.map_err(|error| format!("{error:#}")))
    }

    async fn select_in_active_buffer(
        &mut self,
        version: u64,
        ranges: Vec<ByteRange>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::BufferWrite) {
            return Ok(Err(error.to_string()));
        }
        let Some(editor) = self.lathe.editor() else {
            return Ok(Err("this command was not run from an editor".into()));
        };
        let ranges = match ranges
            .into_iter()
            .map(from_byte_range)
            .collect::<Result<Vec<_>>>()
        {
            Ok(ranges) => ranges,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let result = self
            .on_main_thread(move |cx| {
                async move { editor.select(version, ranges, cx) }.boxed_local()
            })
            .await;
        Ok(result.map_err(|error| format!("{error:#}")))
    }
}

fn to_byte_range(range: Range<usize>) -> ByteRange {
    ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}

fn from_byte_range(range: ByteRange) -> Result<Range<usize>> {
    let start = usize::try_from(range.start).context("range start out of bounds")?;
    let end = usize::try_from(range.end).context("range end out of bounds")?;
    Ok(start..end)
}

fn register_command(state: &mut LatheState, spec: CommandSpec) -> Result<()> {
    if !state.initializing {
        bail!("commands can only be registered while the extension initializes");
    }
    if !is_valid_command_id(&spec.id) {
        bail!(
            "invalid command id {:?}: use 1 to 64 lowercase letters, digits and `-`",
            spec.id
        );
    }
    let title = spec.title.trim();
    if title.is_empty() {
        bail!("command {:?} needs a title", spec.id);
    }
    if state.commands.iter().any(|command| *command.id == spec.id) {
        bail!("command {:?} is already registered", spec.id);
    }
    state.commands.push(LatheCommand {
        id: spec.id.into(),
        title: title.into(),
    });
    Ok(())
}

pub(super) async fn run_command(
    store: &mut Store<WasmState>,
    command_id: &str,
    editor: Option<Arc<dyn LatheEditorDelegate>>,
) -> Result<()> {
    let bindings = store
        .data_mut()
        .lathe
        .bindings
        .take()
        .ok_or_else(|| anyhow!("extension does not use the Lathe API"))?;
    store.data_mut().lathe.editor = editor;
    store.data_mut().lathe.running_command = true;
    let result = match &bindings {
        LatheBindings::V0_1(bindings) => {
            bindings
                .lathe_extension_command_handler()
                .call_run_command(&mut *store, command_id)
                .await
        }
        LatheBindings::V0_2(bindings) => {
            bindings
                .lathe_extension0_1_0_command_handler()
                .call_run_command(&mut *store, command_id)
                .await
        }
    };
    store.data_mut().lathe.running_command = false;
    store.data_mut().lathe.editor = None;
    store.data_mut().lathe.bindings = Some(bindings);
    result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))
}

pub(super) async fn dispatch_buffer_event(
    store: &mut Store<WasmState>,
    event: LatheBufferEvent,
    buffer: Arc<dyn LatheEditorDelegate>,
) -> Result<()> {
    let bindings = store
        .data_mut()
        .lathe
        .bindings
        .take()
        .ok_or_else(|| anyhow!("extension does not use the Lathe API"))?;
    store.data_mut().lathe.editor = Some(buffer);
    let result = match &bindings {
        LatheBindings::V0_1(bindings) => {
            bindings
                .lathe_extension_event_handler()
                .call_on_buffer_event(&mut *store, event.to_wit())
                .await
        }
        LatheBindings::V0_2(bindings) => {
            bindings
                .lathe_extension0_1_0_event_handler()
                .call_on_buffer_event(&mut *store, event.to_wit())
                .await
        }
    };
    store.data_mut().lathe.editor = None;
    store.data_mut().lathe.bindings = Some(bindings);
    result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))
}

/// Delivers a click on one of the extension's views. Treated like running a
/// command, since the user did it: prompts are allowed.
pub(super) async fn dispatch_view_event(
    store: &mut Store<WasmState>,
    view_id: &str,
    element_id: &str,
    event: super::lathe_views::ViewEvent,
) -> Result<()> {
    let bindings = store
        .data_mut()
        .lathe
        .bindings
        .take()
        .ok_or_else(|| anyhow!("extension does not use the Lathe API"))?;
    let LatheBindings::V0_2(v0_2) = &bindings else {
        store.data_mut().lathe.bindings = Some(bindings);
        bail!("views need an extension built with lathe_extension_api 0.2");
    };
    store.data_mut().lathe.running_command = true;
    let result = v0_2
        .lathe_extension0_2_0_view_handler()
        .call_on_view_event(&mut *store, view_id, element_id, &to_wit_view_event(event))
        .await;
    store.data_mut().lathe.running_command = false;
    store.data_mut().lathe.bindings = Some(bindings);
    result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))
}

fn to_wit_view_event(
    event: super::lathe_views::ViewEvent,
) -> v0_2_0::lathe::extension0_2_0::views::ViewEvent {
    use super::lathe_views::ViewEvent;
    use v0_2_0::lathe::extension0_2_0::views::ViewEvent as WitViewEvent;
    match event {
        ViewEvent::Clicked => WitViewEvent::Clicked,
        ViewEvent::Toggled(on) => WitViewEvent::Toggled(on),
        ViewEvent::InputChanged(text) => WitViewEvent::InputChanged(text),
        ViewEvent::Submitted(text) => WitViewEvent::Submitted(text),
        ViewEvent::MenuChosen(id) => WitViewEvent::MenuChosen(id),
        ViewEvent::Selected(value) => WitViewEvent::Selected(value),
    }
}

fn to_wit_surface_event(
    event: LatheSurfaceEvent,
) -> v0_2_0::lathe::extension0_2_0::surfaces::SurfaceEvent {
    use v0_2_0::lathe::extension0_2_0::surfaces::{PreviewFile, SurfaceEvent};
    match event {
        LatheSurfaceEvent::ViewShown(view_id) => SurfaceEvent::ViewShown(view_id.to_string()),
        LatheSurfaceEvent::ViewHidden(view_id) => SurfaceEvent::ViewHidden(view_id.to_string()),
        LatheSurfaceEvent::StatusItemClicked => SurfaceEvent::StatusItemClicked,
        LatheSurfaceEvent::PreviewOpened {
            preview_id,
            view_id,
            path,
            abs_path,
        } => SurfaceEvent::PreviewOpened(PreviewFile {
            preview_id: preview_id.to_string(),
            view_id: view_id.to_string(),
            path,
            abs_path,
        }),
        LatheSurfaceEvent::PreviewChanged(view_id) => {
            SurfaceEvent::PreviewChanged(view_id.to_string())
        }
        LatheSurfaceEvent::PreviewClosed(view_id) => {
            SurfaceEvent::PreviewClosed(view_id.to_string())
        }
    }
}

/// Delivers a surface event. A status item click counts as a user action, so
/// the handler may prompt and open files. `buffer` is what `active-buffer`
/// returns while the handler runs: the previewed file for `preview-opened`.
pub(super) async fn dispatch_surface_event(
    store: &mut Store<WasmState>,
    event: LatheSurfaceEvent,
    buffer: Option<Arc<dyn LatheEditorDelegate>>,
) -> Result<()> {
    let bindings = store
        .data_mut()
        .lathe
        .bindings
        .take()
        .ok_or_else(|| anyhow!("extension does not use the Lathe API"))?;
    let LatheBindings::V0_2(v0_2) = &bindings else {
        store.data_mut().lathe.bindings = Some(bindings);
        bail!("surface events need an extension built with lathe_extension_api 0.2");
    };
    if let LatheSurfaceEvent::PreviewClosed(view_id) = &event {
        store.data_mut().lathe.view_ids.remove(view_id);
    }
    store.data_mut().lathe.running_command = event.is_user_action();
    store.data_mut().lathe.editor = buffer;
    let result = v0_2
        .lathe_extension0_2_0_view_handler()
        .call_on_surface_event(&mut *store, &to_wit_surface_event(event))
        .await;
    store.data_mut().lathe.editor = None;
    store.data_mut().lathe.running_command = false;
    store.data_mut().lathe.bindings = Some(bindings);
    result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))
}

fn to_tone(tone: v0_2_0::lathe::extension0_2_0::views::Tone) -> super::lathe_views::Tone {
    use super::lathe_views::Tone;
    use v0_2_0::lathe::extension0_2_0::views::Tone as WitTone;
    match tone {
        WitTone::Neutral => Tone::Neutral,
        WitTone::Accent => Tone::Accent,
        WitTone::Success => Tone::Success,
        WitTone::Warning => Tone::Warning,
        WitTone::Error => Tone::Error,
    }
}

fn to_list_item(
    item: v0_2_0::lathe::extension0_2_0::views::ListItemNode,
) -> super::lathe_views::NodeKind {
    use super::lathe_views::{Disclosure, MenuEntry, NodeKind};
    use v0_2_0::lathe::extension0_2_0::views as wit_views;
    NodeKind::ListItem {
        id: item.id.into(),
        label: item.label.into(),
        detail: item.detail.map(Into::into),
        icon: item.icon.map(Into::into),
        tooltip: item.tooltip.map(Into::into),
        selected: item.selected,
        disabled: item.disabled,
        indent: item.indent,
        disclosure: match item.disclosure {
            wit_views::Disclosure::None => Disclosure::None,
            wit_views::Disclosure::Collapsed => Disclosure::Collapsed,
            wit_views::Disclosure::Expanded => Disclosure::Expanded,
        },
        menu: item
            .menu
            .into_iter()
            .map(|entry| MenuEntry {
                id: entry.id.into(),
                label: entry.label.into(),
            })
            .collect(),
    }
}

/// Asks the extension for rows of a virtual list and checks them.
pub(super) async fn provide_rows(
    store: &mut Store<WasmState>,
    view_id: &str,
    list_id: &str,
    start: u32,
    count: u32,
) -> Result<Vec<super::lathe_views::NodeKind>> {
    let count = count.min(super::lathe_views::ROWS_PER_REQUEST);
    let bindings = store
        .data_mut()
        .lathe
        .bindings
        .take()
        .ok_or_else(|| anyhow!("extension does not use the Lathe API"))?;
    let LatheBindings::V0_2(v0_2) = &bindings else {
        store.data_mut().lathe.bindings = Some(bindings);
        bail!("virtual lists need an extension built with lathe_extension_api 0.2");
    };
    let result = v0_2
        .lathe_extension0_2_0_view_handler()
        .call_provide_rows(&mut *store, view_id, list_id, start, count)
        .await;
    store.data_mut().lathe.bindings = Some(bindings);
    let rows = result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))?
        .into_iter()
        .map(to_list_item)
        .collect::<Vec<_>>();
    super::lathe_views::validate_rows(&rows, count)?;
    Ok(rows)
}

fn to_lathe_view(
    view: v0_2_0::lathe::extension0_2_0::views::View,
) -> Result<super::lathe_views::LatheView> {
    use super::lathe_views::{Node, NodeKind, SelectOption, TextStyle, validate};
    use v0_2_0::lathe::extension0_2_0::views as wit_views;
    let nodes = view
        .nodes
        .into_iter()
        .map(|node| Node {
            kind: match node.kind {
                wit_views::NodeKind::Column => NodeKind::Column,
                wit_views::NodeKind::Row => NodeKind::Row,
                wit_views::NodeKind::Scroll => NodeKind::Scroll,
                wit_views::NodeKind::Text(text) => NodeKind::Text {
                    text: text.text.into(),
                    style: match text.style {
                        wit_views::TextStyle::Body => TextStyle::Body,
                        wit_views::TextStyle::Muted => TextStyle::Muted,
                        wit_views::TextStyle::Heading => TextStyle::Heading,
                        wit_views::TextStyle::Code => TextStyle::Code,
                    },
                    tone: to_tone(text.tone),
                },
                wit_views::NodeKind::Button(button) => NodeKind::Button {
                    id: button.id.into(),
                    label: button.label.into(),
                    icon: button.icon.map(Into::into),
                    tooltip: button.tooltip.map(Into::into),
                    disabled: button.disabled,
                },
                wit_views::NodeKind::ListItem(item) => to_list_item(item),
                wit_views::NodeKind::VirtualList(list) => NodeKind::VirtualList {
                    id: list.id.into(),
                    row_count: list.row_count,
                    revision: list.revision,
                },
                wit_views::NodeKind::TextInput(input) => NodeKind::TextInput {
                    id: input.id.into(),
                    value: input.value.into(),
                    placeholder: input.placeholder.map(Into::into),
                    disabled: input.disabled,
                },
                wit_views::NodeKind::Checkbox(checkbox) => NodeKind::Checkbox {
                    id: checkbox.id.into(),
                    label: checkbox.label.into(),
                    checked: checkbox.checked,
                    disabled: checkbox.disabled,
                },
                wit_views::NodeKind::Badge(badge) => NodeKind::Badge {
                    text: badge.text.into(),
                    tone: to_tone(badge.tone),
                },
                wit_views::NodeKind::Select(select) => NodeKind::Select {
                    id: select.id.into(),
                    options: select
                        .options
                        .into_iter()
                        .map(|option| SelectOption {
                            value: option.value.into(),
                            label: option.label.into(),
                        })
                        .collect(),
                    selected: select.selected.map(Into::into),
                    placeholder: select.placeholder.map(Into::into),
                    disabled: select.disabled,
                },
                wit_views::NodeKind::Progress(progress) => NodeKind::Progress {
                    fraction: progress.fraction,
                    label: progress.label.map(Into::into),
                },
                wit_views::NodeKind::Markdown(markdown) => NodeKind::Markdown(markdown.into()),
                wit_views::NodeKind::Icon(name) => NodeKind::Icon(name.into()),
                wit_views::NodeKind::Divider => NodeKind::Divider,
                wit_views::NodeKind::Spacer => NodeKind::Spacer,
            },
            children: node.children,
        })
        .collect();
    validate(nodes)
}

impl v0_2_0::lathe::extension0_2_0::views::Host for WasmState {
    async fn update_view(
        &mut self,
        view_id: String,
        view: v0_2_0::lathe::extension0_2_0::views::View,
    ) -> wasmtime::Result<Result<(), String>> {
        let prepared = (|| {
            self.lathe.require(LatheCapability::UiViews)?;
            super::lathe_views::validate_id("view", &view_id)?;
            let view_id: Arc<str> = view_id.as_str().into();
            if !self.lathe.view_ids.contains(&view_id)
                && self.lathe.view_ids.len() >= super::lathe_views::MAX_VIEWS_PER_EXTENSION
            {
                bail!(
                    "an extension can have at most {} views",
                    super::lathe_views::MAX_VIEWS_PER_EXTENSION
                );
            }
            let view = to_lathe_view(view)?;
            self.lathe.view_ids.insert(view_id.clone());
            anyhow::Ok((view_id, view))
        })();
        let (view_id, view) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(format!("{error:#}"))),
        };
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Ok(()));
        };
        let extension_id = self.manifest.id.clone();
        self.on_main_thread(move |cx| {
            async move { ui.update_view(extension_id, view_id, view, cx) }.boxed_local()
        })
        .await;
        Ok(Ok(()))
    }

    async fn open_location(
        &mut self,
        location: v0_2_0::lathe::extension0_2_0::views::Location,
    ) -> wasmtime::Result<Result<(), String>> {
        if !self.lathe.running_command {
            return Ok(Err(
                "files can only be opened while handling something the user \
                           did: a command, a view event or a status item click"
                    .into(),
            ));
        }
        if location.path.trim().is_empty() {
            return Ok(Err("open-location needs a path".into()));
        }
        if location.line == Some(0) || location.column == Some(0) {
            return Ok(Err("line and column are 1-based".into()));
        }
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Err(
                "opening files is not available in this build of Lathe".into()
            ));
        };
        let location = LatheLocation {
            path: location.path,
            row: location.line.map(|line| line - 1),
            column: location.column.map(|column| column - 1),
        };
        let result = self
            .on_main_thread(move |cx| async move { ui.open_location(location, cx) }.boxed_local())
            .await;
        Ok(result.map_err(|error| format!("{error:#}")))
    }

    async fn remove_view(&mut self, view_id: String) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::UiViews) {
            return Ok(Err(error.to_string()));
        }
        let view_id: Arc<str> = view_id.as_str().into();
        if !self.lathe.view_ids.remove(&view_id) {
            return Ok(Ok(()));
        }
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Ok(()));
        };
        let extension_id = self.manifest.id.clone();
        self.on_main_thread(move |cx| {
            async move { ui.remove_view(extension_id, view_id, cx) }.boxed_local()
        })
        .await;
        Ok(Ok(()))
    }
}

impl v0_2_0::lathe::extension0_2_0::surfaces::Host for WasmState {
    async fn register_panel(
        &mut self,
        spec: v0_2_0::lathe::extension0_2_0::surfaces::PanelSpec,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(register_panel(&mut self.lathe, spec).map_err(|error| error.to_string()))
    }

    async fn register_preview(
        &mut self,
        spec: v0_2_0::lathe::extension0_2_0::surfaces::PreviewSpec,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(register_preview(&mut self.lathe, spec).map_err(|error| error.to_string()))
    }

    async fn set_status_item(
        &mut self,
        item: Option<v0_2_0::lathe::extension0_2_0::surfaces::StatusItem>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::UiStatus) {
            return Ok(Err(error.to_string()));
        }
        let item = match item.map(to_status_item).transpose() {
            Ok(item) => item,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Ok(()));
        };
        let extension_id = self.manifest.id.clone();
        self.on_main_thread(move |cx| {
            async move { ui.set_status_item(extension_id, item, cx) }.boxed_local()
        })
        .await;
        Ok(Ok(()))
    }
}

fn validate_title(what: &str, id: &str, title: &str) -> Result<Arc<str>> {
    let title = title.trim();
    if title.is_empty() {
        bail!("{what} {id:?} needs a title");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        bail!("{what} {id:?} has a title longer than {MAX_TITLE_CHARS} characters");
    }
    Ok(title.into())
}

fn validate_surface_id(state: &LatheState, what: &str, id: &str) -> Result<()> {
    if !state.initializing {
        bail!("{what}s can only be registered while the extension initializes");
    }
    // No capability check here: revoking `ui:views` reloads the extension,
    // and failing registration would break its startup. Surfaces check the
    // grant when they show anything instead.
    if !is_valid_command_id(id) {
        bail!("invalid {what} id {id:?}: use 1 to 64 lowercase letters, digits and `-`");
    }
    Ok(())
}

fn register_panel(
    state: &mut LatheState,
    spec: v0_2_0::lathe::extension0_2_0::surfaces::PanelSpec,
) -> Result<()> {
    use v0_2_0::lathe::extension0_2_0::surfaces::DockSide;
    validate_surface_id(state, "panel", &spec.id)?;
    let title = validate_title("panel", &spec.id, &spec.title)?;
    if let Some(icon) = &spec.icon {
        super::lathe_views::validate_id("panel icon", icon)?;
    }
    if state.panels.iter().any(|panel| *panel.id == spec.id) {
        bail!("panel {:?} is already registered", spec.id);
    }
    if state.panels.len() >= MAX_PANELS_PER_EXTENSION {
        bail!("an extension can have at most {MAX_PANELS_PER_EXTENSION} panels");
    }
    state.panels.push(LathePanel {
        id: spec.id.into(),
        title,
        icon: spec.icon.map(Into::into),
        dock: match spec.dock {
            DockSide::Left => LatheDockSide::Left,
            DockSide::Right => LatheDockSide::Right,
            DockSide::Bottom => LatheDockSide::Bottom,
        },
    });
    Ok(())
}

fn register_preview(
    state: &mut LatheState,
    spec: v0_2_0::lathe::extension0_2_0::surfaces::PreviewSpec,
) -> Result<()> {
    validate_surface_id(state, "preview", &spec.id)?;
    let title = validate_title("preview", &spec.id, &spec.title)?;
    if spec.file_suffixes.is_empty() || spec.file_suffixes.len() > MAX_PREVIEW_SUFFIXES {
        bail!(
            "preview {:?} needs 1 to {MAX_PREVIEW_SUFFIXES} file suffixes",
            spec.id
        );
    }
    for suffix in &spec.file_suffixes {
        if suffix.is_empty()
            || suffix.chars().count() > MAX_SUFFIX_CHARS
            || suffix.contains(['/', '\\'])
        {
            bail!(
                "invalid file suffix {suffix:?} for preview {:?}: use up to \
                 {MAX_SUFFIX_CHARS} characters and no path separators",
                spec.id
            );
        }
    }
    if state.previews.iter().any(|preview| *preview.id == spec.id) {
        bail!("preview {:?} is already registered", spec.id);
    }
    if state.previews.len() >= MAX_PREVIEWS_PER_EXTENSION {
        bail!("an extension can have at most {MAX_PREVIEWS_PER_EXTENSION} previews");
    }
    state.previews.push(LathePreview {
        id: spec.id.into(),
        title,
        file_suffixes: spec.file_suffixes.into_iter().map(Into::into).collect(),
    });
    Ok(())
}

fn to_status_item(
    item: v0_2_0::lathe::extension0_2_0::surfaces::StatusItem,
) -> Result<LatheStatusItem> {
    let label = truncate_chars(item.label.trim(), MAX_STATUS_CHARS);
    if label.is_empty() && item.icon.is_none() {
        bail!("a status item needs a label or an icon");
    }
    if let Some(icon) = &item.icon {
        super::lathe_views::validate_id("status item icon", icon)?;
    }
    Ok(LatheStatusItem {
        label: label.into(),
        icon: item.icon.map(Into::into),
        tooltip: item
            .tooltip
            .map(|tooltip| truncate_chars(&tooltip, MAX_NOTIFICATION_CHARS).into()),
        tone: to_tone(item.tone),
    })
}

impl v0_2_0::lathe::extension0_2_0::decorations::Host for WasmState {
    async fn set_decorations(
        &mut self,
        version: u64,
        layer: String,
        decorations: Vec<v0_2_0::lathe::extension0_2_0::decorations::Decoration>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::UiDecorations) {
            return Ok(Err(error.to_string()));
        }
        let Some(editor) = self.lathe.editor() else {
            return Ok(Err(
                "decorations need a buffer: call this from a command run in an \
                           editor, or from a buffer event"
                    .into(),
            ));
        };
        let decorations = match to_decorations(&layer, decorations) {
            Ok(decorations) => decorations,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let extension_id = self.manifest.id.clone();
        let layer: Arc<str> = layer.into();
        let result = self
            .on_main_thread(move |cx| {
                async move { editor.set_decorations(version, extension_id, layer, decorations, cx) }
                    .boxed_local()
            })
            .await;
        Ok(result.map_err(|error| format!("{error:#}")))
    }
}

fn to_decorations(
    layer: &str,
    decorations: Vec<v0_2_0::lathe::extension0_2_0::decorations::Decoration>,
) -> Result<Vec<LatheDecoration>> {
    use v0_2_0::lathe::extension0_2_0::decorations::DecorationStyle;
    if !is_valid_command_id(layer) {
        bail!("invalid decoration layer {layer:?}: use 1 to 64 lowercase letters, digits and `-`");
    }
    if decorations.len() > MAX_DECORATIONS_PER_LAYER {
        bail!("a decoration layer can have at most {MAX_DECORATIONS_PER_LAYER} decorations");
    }
    decorations
        .into_iter()
        .map(|decoration| {
            let range = from_byte_range(decoration.range)?;
            let message = match decoration.message {
                Some(message) if message.chars().count() > MAX_DECORATION_MESSAGE_CHARS => bail!(
                    "decoration messages can be at most {MAX_DECORATION_MESSAGE_CHARS} characters"
                ),
                Some(message) if message.trim().is_empty() => None,
                Some(message) => Some(message.trim().into()),
                None => None,
            };
            Ok(LatheDecoration {
                range,
                style: match decoration.style {
                    DecorationStyle::Background => LatheDecorationStyle::Background,
                    DecorationStyle::Underline => LatheDecorationStyle::Underline,
                    DecorationStyle::WavyUnderline => LatheDecorationStyle::WavyUnderline,
                    DecorationStyle::Gutter => LatheDecorationStyle::Gutter,
                },
                tone: to_tone(decoration.tone),
                message,
            })
        })
        .collect()
}

impl lathe::extension::events::Host for WasmState {}

impl lathe::extension::ui::Host for WasmState {
    async fn set_status_text(
        &mut self,
        text: Option<String>,
        tooltip: Option<String>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::UiStatus) {
            return Ok(Err(error.to_string()));
        }
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Ok(()));
        };
        let text = text
            .map(|text| truncate_chars(text.trim(), MAX_STATUS_CHARS))
            .filter(|text| !text.is_empty());
        let extension_id = self.manifest.id.clone();
        self.on_main_thread(move |cx| {
            async move { ui.set_status_text(extension_id, text, tooltip, cx) }.boxed_local()
        })
        .await;
        Ok(Ok(()))
    }

    async fn show_notification(
        &mut self,
        level: NotificationLevel,
        message: String,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Err(error) = self.lathe.require(LatheCapability::UiStatus) {
            return Ok(Err(error.to_string()));
        }
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Ok(()));
        };
        let extension_name: Arc<str> = self.manifest.name.as_str().into();
        let message = truncate_chars(&message, MAX_NOTIFICATION_CHARS);
        self.on_main_thread(move |cx| {
            async move { ui.show_notification(extension_name, level, message, cx) }.boxed_local()
        })
        .await;
        Ok(Ok(()))
    }

    async fn prompt_input(
        &mut self,
        label: String,
        placeholder: Option<String>,
    ) -> wasmtime::Result<Result<Option<String>, String>> {
        let ui = match self.prompt_ui() {
            Ok(ui) => ui,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let extension_name: Arc<str> = self.manifest.name.as_str().into();
        let receiver = self
            .on_main_thread(move |cx| {
                async move { ui.prompt_input(extension_name, label, placeholder, cx) }.boxed_local()
            })
            .await;
        Ok(match receiver {
            Ok(receiver) => Ok(receiver.await.unwrap_or(None)),
            Err(error) => Err(format!("{error:#}")),
        })
    }

    async fn quick_pick(
        &mut self,
        label: String,
        items: Vec<PickItem>,
    ) -> wasmtime::Result<Result<Option<String>, String>> {
        let ui = match self.prompt_ui() {
            Ok(ui) => ui,
            Err(error) => return Ok(Err(error.to_string())),
        };
        if items.is_empty() {
            return Ok(Ok(None));
        }
        let extension_name: Arc<str> = self.manifest.name.as_str().into();
        let receiver = self
            .on_main_thread(move |cx| {
                async move { ui.quick_pick(extension_name, label, items, cx) }.boxed_local()
            })
            .await;
        Ok(match receiver {
            Ok(receiver) => Ok(receiver.await.unwrap_or(None)),
            Err(error) => Err(format!("{error:#}")),
        })
    }
}

impl lathe::extension::tasks::Host for WasmState {
    async fn spawn_task(&mut self, task: TaskSpec) -> wasmtime::Result<Result<(), String>> {
        let task = match self.prepare_task(task) {
            Ok(task) => task,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let Some(ui) = self.host.lathe_ui() else {
            return Ok(Err("tasks are not available in this build of Lathe".into()));
        };
        let result = self
            .on_main_thread(move |cx| async move { ui.spawn_task(task, cx) }.boxed_local())
            .await;
        Ok(result.map_err(|error| format!("{error:#}")))
    }
}

impl lathe::extension::extension_settings::Host for WasmState {
    /// Reads the extension's section for the file the running command or
    /// event is about, so a trusted project's settings apply to its own files.
    /// Untrusted projects' settings files are never applied by the settings
    /// store, so they cannot reach extensions through here.
    async fn get(&mut self) -> wasmtime::Result<Result<Option<String>, String>> {
        let extension_id = self.manifest.id.clone();
        let editor = self.lathe.editor();
        let value = self
            .on_main_thread(move |cx| {
                async move {
                    let location = editor.and_then(|editor| editor.settings_location(cx));
                    cx.update(|cx| {
                        let location = location.as_ref().map(|(worktree_id, path)| {
                            settings::SettingsLocation {
                                worktree_id: *worktree_id,
                                path: path.as_ref(),
                            }
                        });
                        <crate::ExtensionSettings as settings::Settings>::get(location, cx)
                            .extension_settings
                            .get(&extension_id)
                            .cloned()
                    })
                }
                .boxed_local()
            })
            .await;
        Ok(value
            .map(|value| serde_json::to_string(&value))
            .transpose()
            .map_err(|error| error.to_string()))
    }
}

impl WasmState {
    fn prepare_task(&self, mut task: TaskSpec) -> Result<TaskSpec> {
        self.lathe.require(LatheCapability::TasksSpawn)?;
        if task.command.trim().is_empty() {
            bail!("a task needs a command");
        }
        if let Some(cwd) = &task.cwd
            && !std::path::Path::new(cwd).is_absolute()
        {
            bail!("a task's cwd must be an absolute path, got {cwd:?}");
        }
        let label = task.label.trim();
        let label = if label.is_empty() {
            task.command.as_str()
        } else {
            label
        };
        task.label = truncate_chars(
            &format!("{}: {label}", self.manifest.name),
            MAX_STATUS_CHARS,
        );
        Ok(task)
    }
}

const MAX_STATUS_CHARS: usize = 60;
const MAX_NOTIFICATION_CHARS: usize = 500;

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => format!("{}…", &text[..byte_index]),
        None => text.to_string(),
    }
}

impl WasmState {
    fn prompt_ui(&self) -> Result<Arc<dyn LatheUiDelegate>> {
        self.lathe.require(LatheCapability::UiPrompt)?;
        if !self.lathe.running_command {
            bail!("prompts can only be shown while a command runs");
        }
        self.host
            .lathe_ui()
            .context("prompts are not available in this build of Lathe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, title: &str) -> CommandSpec {
        CommandSpec {
            id: id.into(),
            title: title.into(),
        }
    }

    #[test]
    fn test_register_command_validation() {
        let mut state = LatheState {
            initializing: true,
            ..Default::default()
        };

        assert!(register_command(&mut state, spec("say-hello", " Say Hello ")).is_ok());
        assert_eq!(&*state.commands[0].title, "Say Hello");
        assert!(register_command(&mut state, spec("say-hello", "Again")).is_err());
        assert!(register_command(&mut state, spec("Say_Hello", "Bad id")).is_err());
        assert!(register_command(&mut state, spec("", "Empty id")).is_err());
        assert!(register_command(&mut state, spec("no-title", "  ")).is_err());

        state.initializing = false;
        assert!(register_command(&mut state, spec("late", "Too Late")).is_err());
        assert_eq!(state.commands.len(), 1);
    }

    #[test]
    fn test_register_panels_and_previews() {
        use v0_2_0::lathe::extension0_2_0::surfaces::{DockSide, PanelSpec, PreviewSpec};
        let panel = |id: &str, title: &str| PanelSpec {
            id: id.into(),
            title: title.into(),
            icon: None,
            dock: DockSide::Left,
        };
        let preview = |id: &str, suffixes: &[&str]| PreviewSpec {
            id: id.into(),
            title: "Preview".into(),
            file_suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
        };
        let mut state = LatheState::default();
        assert!(
            register_panel(&mut state, panel("a", "A"))
                .unwrap_err()
                .to_string()
                .contains("while the extension initializes")
        );
        state.initializing = true;
        register_panel(&mut state, panel("a", "A")).unwrap();
        assert!(
            register_panel(&mut state, panel("a", "A")).is_err(),
            "duplicate"
        );
        assert!(register_panel(&mut state, panel("Bad Id", "A")).is_err());
        assert!(
            register_panel(&mut state, panel("b", "  ")).is_err(),
            "blank title"
        );
        for id in ["b", "c", "d"] {
            register_panel(&mut state, panel(id, id)).unwrap();
        }
        assert!(
            register_panel(&mut state, panel("e", "E"))
                .unwrap_err()
                .to_string()
                .contains("at most")
        );

        register_preview(&mut state, preview("csv", &[".csv", "Dockerfile"])).unwrap();
        assert!(register_preview(&mut state, preview("none", &[])).is_err());
        assert!(register_preview(&mut state, preview("slash", &["a/b"])).is_err());
        assert!(register_preview(&mut state, preview("empty", &[""])).is_err());
        assert!(state.previews[0].matches("x.csv") && state.previews[0].matches("Dockerfile"));
        assert!(!state.previews[0].matches("x.csv.bak"));
    }

    #[test]
    fn test_validate_ranges() {
        let text = "héllo";
        assert!(validate_ranges(text, &[0..1, 1..3, 6..6], true).is_ok());
        assert!(validate_ranges(text, &[0..2], true).is_err(), "inside é");
        assert!(
            validate_ranges(text, &[0..7], true).is_err(),
            "past the end"
        );
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = [3..1];
        assert!(validate_ranges(text, &reversed, true).is_err());
        assert!(
            validate_ranges(text, &[0..3, 1..4], true).is_err(),
            "overlap"
        );
        assert!(
            validate_ranges(text, &[3..4, 0..1], true).is_err(),
            "unsorted"
        );
        assert!(
            validate_ranges(text, &[3..4, 0..1], false).is_ok(),
            "selections may be in any order"
        );
    }

    #[test]
    fn test_grants() {
        let mut grants = LatheGrants::default();
        grants.grant("a".into(), [LatheCapability::BufferRead]);
        grants.grant("a".into(), [LatheCapability::UiStatus]);
        assert_eq!(
            grants.granted("a"),
            BTreeSet::from_iter([LatheCapability::BufferRead, LatheCapability::UiStatus])
        );
        assert!(grants.granted("b").is_empty());

        let round_tripped: LatheGrants =
            serde_json::from_str(&serde_json::to_string(&grants).unwrap()).unwrap();
        assert_eq!(round_tripped, grants);

        grants
            .extensions
            .get_mut("a")
            .unwrap()
            .insert("from-a-newer-lathe".into());
        assert_eq!(grants.granted("a").len(), 2, "unknown names are ignored");

        grants.revoke("a");
        assert!(grants.granted("a").is_empty());
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        assert_eq!(truncate_chars("héllo", 2), "hé…");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn test_capability_parsing() {
        let capabilities =
            LatheCapability::parse_all(&["buffer:read".into(), "buffer:write".into()]).unwrap();
        assert_eq!(capabilities.len(), 2);
        assert!(LatheCapability::parse_all(&["buffer:delete".into()]).is_err());
    }

    #[test]
    fn test_api_version_gating() {
        let version = Version::new(0, 1, 0);
        assert!(is_supported_api_version(ReleaseChannel::Beta, &version));
        assert!(
            is_supported_api_version(ReleaseChannel::Beta, &Version::new(0, 2, 3)),
            "0.2.x extensions load"
        );
        assert!(is_supported_api_version(ReleaseChannel::Dev, &version));
        assert!(
            is_supported_api_version(ReleaseChannel::Beta, &Version::new(0, 1, 7)),
            "a patch release of lathe_extension_api still loads"
        );
        assert!(!is_supported_api_version(
            ReleaseChannel::Beta,
            &Version::new(0, 3, 0)
        ));
    }
}
