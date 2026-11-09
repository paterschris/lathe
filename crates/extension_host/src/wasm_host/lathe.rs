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

/// The Lathe API version this host implements. Extensions embed the version
/// of `lathe_extension_api` they were built with; any 0.1.x is accepted,
/// because patch releases of the crate may only change guest-side code and
/// never the 0.1.0 WIT.
pub const API_VERSION: Version = Version::new(0, 1, 0);

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

/// Whether this build accepts extensions that use the given Lathe API version.
pub fn is_supported_api_version(release_channel: ReleaseChannel, version: &Version) -> bool {
    version.major == API_VERSION.major
        && version.minor == API_VERSION.minor
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
        store.data_mut().lathe.bindings = Some(load_bindings(store, &instance)?);
        super::wit::Extension::V0_8_0(extension)
    } else if *zed_api_version >= since_v0_6_0::MIN_VERSION {
        let instance = linker_v0_6_0(executor)
            .instantiate_async(&mut *store, component)
            .await
            .map_err(anyhow::Error::from)
            .context("failed to instantiate Lathe extension")?;
        let extension =
            since_v0_6_0::Extension::new(&mut *store, &instance).map_err(anyhow::Error::from)?;
        store.data_mut().lathe.bindings = Some(load_bindings(store, &instance)?);
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
) -> Result<Extension> {
    Extension::new(&mut *store, instance)
        .map_err(anyhow::Error::from)
        .context("extension declares the Lathe API but does not implement it")
}

fn add_lathe_imports(linker: &mut Linker<WasmState>) -> wasmtime::Result<()> {
    Extension::add_to_linker::<_, WasmState>(linker, |state| state)
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
    pub(super) bindings: Option<Extension>,
    /// Commands may only be registered while the extension initializes.
    pub(super) initializing: bool,
    pub(super) commands: Vec<LatheCommand>,
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
    let result = bindings
        .lathe_extension_command_handler()
        .call_run_command(&mut *store, command_id)
        .await;
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
    let result = bindings
        .lathe_extension_event_handler()
        .call_on_buffer_event(&mut *store, event.to_wit())
        .await;
    store.data_mut().lathe.editor = None;
    store.data_mut().lathe.bindings = Some(bindings);
    result
        .map_err(anyhow::Error::from)?
        .map_err(|error| anyhow!(error))
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
        assert!(is_supported_api_version(ReleaseChannel::Dev, &version));
        assert!(
            is_supported_api_version(ReleaseChannel::Beta, &Version::new(0, 1, 7)),
            "a patch release of lathe_extension_api still loads"
        );
        assert!(!is_supported_api_version(
            ReleaseChannel::Beta,
            &Version::new(0, 2, 0)
        ));
    }
}
