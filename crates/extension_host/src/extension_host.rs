mod capability_granter;
pub mod extension_settings;
pub mod headless_host;
pub mod registry;
pub mod wasm_host;

#[cfg(test)]
mod extension_store_test;

use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use client::{Client, proto, telemetry::Telemetry};
use cloud_api_types::{ExtensionMetadata, ExtensionProvides, GetExtensionsResponse};
use collections::{BTreeMap, BTreeSet, FxHashSet, HashMap, HashSet, btree_map};
pub use extension::ExtensionManifest;
use extension::extension_builder::{CompileExtensionOptions, ExtensionBuilder};
use extension::{
    ExtensionContextServerProxy, ExtensionDebugAdapterProviderProxy, ExtensionEvents,
    ExtensionGrammarProxy, ExtensionHostProxy, ExtensionLanguageProxy,
    ExtensionLanguageServerProxy, ExtensionSnippetProxy, ExtensionThemeProxy,
};
use fs::{Fs, RemoveOptions, RenameOptions};
use futures::future::{Shared, join_all};
use futures::{
    AsyncReadExt as _, Future, FutureExt as _, StreamExt as _,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    io::BufReader,
    select_biased,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, Subscription,
    Task, TaskExt, UpdateGlobal as _, WeakEntity, actions,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl};
use language::{
    LanguageConfig, LanguageMatcher, LanguageName, LanguageQueries, LoadedLanguage, QueryFile,
    QueryFileContents, QueryFiles, Rope,
};
use node_runtime::NodeRuntime;
use project::{ContextProviderWithTasks, Project};
use release_channel::ReleaseChannel;
use remote::{ConnectionState, RemoteClient, RemoteClientEvent};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{SemanticTokenRules, Settings, SettingsStore};
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::{
    borrow::Cow,
    cmp::Ordering,
    path::{self, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use task::TaskTemplates;
use url::Url;
use util::{
    PathExt, ResultExt,
    paths::{PathStyle, RemotePathBuf},
};
use wasm_host::{
    WasmExtension, WasmHost,
    wit::{is_supported_wasm_api_version, wasm_api_version_range},
};

pub use extension::{
    ExtensionLibraryKind, GrammarManifestEntry, OldExtensionManifest, SchemaVersion,
};
pub use extension_settings::ExtensionSettings;

use crate::headless_host::hash_directory_contents;
use crate::registry::{InstallOrigin, InstallOrigins, LatheManifestFields, RegistryEntry};

pub const RELOAD_DEBOUNCE_DURATION: Duration = Duration::from_millis(200);
const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);
pub(crate) const REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(1);
pub(crate) const MAX_REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(60);
pub(crate) const MAX_REMOTE_SYNC_ATTEMPTS: usize = 10;
pub(crate) const REMOTE_SYNC_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub(crate) fn remote_sync_retry_delay(attempts: usize) -> Duration {
    let exponential = REMOTE_SYNC_RETRY_DELAY * 2u32.saturating_pow(attempts.min(30) as u32);
    exponential.min(MAX_REMOTE_SYNC_RETRY_DELAY)
}

async fn with_remote_sync_timeout<T>(
    cx: &AsyncApp,
    timeout: Duration,
    description: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let timer = cx.background_executor().timer(timeout).fuse();
    let future = future.fuse();
    futures::pin_mut!(timer, future);
    select_biased! {
        result = future => result,
        _ = timer => anyhow::bail!("timed out after {timeout:?} while {description}"),
    }
}

/// Extensions that should no longer be loaded or downloaded.
///
/// These snippets should no longer be downloaded or loaded, because their
/// functionality has been integrated into the core editor.
static SUPPRESSED_EXTENSIONS: LazyLock<FxHashSet<&str>> = LazyLock::new(|| {
    FxHashSet::from_iter([
        "snippets",
        "ruff",
        "ty",
        "basedpyright",
        "basher",
        // ACP
        "opencode",
        "mistral-vibe",
        "auggie",
        "stakpak",
        "codebuddy",
        "autohand-acp",
        "corust-agent",
        "factory-droid",
        "qqcode",
    ])
});

fn provides_filter_set(provides_filter: Option<&str>) -> Option<BTreeSet<ExtensionProvides>> {
    let provides_filter = provides_filter?;
    Some(
        provides_filter
            .split(',')
            .filter_map(|provides| ExtensionProvides::from_str(provides.trim()).ok())
            .collect(),
    )
}

/// Returns the [`SchemaVersion`] range that is compatible with this version of Zed.
pub fn schema_version_range() -> RangeInclusive<SchemaVersion> {
    SchemaVersion::ZERO..=SchemaVersion::CURRENT
}

/// Returns whether the given extension version is compatible with this version of Zed.
pub fn is_version_compatible(
    release_channel: ReleaseChannel,
    extension_version: &ExtensionMetadata,
) -> bool {
    let schema_version = extension_version.manifest.schema_version.unwrap_or(0);
    if SchemaVersion::CURRENT.0 < schema_version {
        return false;
    }

    if let Some(wasm_api_version) = extension_version
        .manifest
        .wasm_api_version
        .as_ref()
        .and_then(|wasm_api_version| Version::from_str(wasm_api_version).ok())
        && !is_supported_wasm_api_version(release_channel, wasm_api_version)
    {
        return false;
    }

    true
}

pub struct ExtensionStore {
    pub proxy: Arc<ExtensionHostProxy>,
    pub builder: Arc<ExtensionBuilder>,
    pub extension_index: ExtensionIndex,
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<HttpClientWithUrl>,
    pub telemetry: Option<Arc<Telemetry>>,
    pub reload_tx: UnboundedSender<Option<Arc<str>>>,
    pub reload_complete_senders: Vec<oneshot::Sender<()>>,
    pub installed_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub outstanding_operations: BTreeMap<Arc<str>, ExtensionOperation>,
    pub index_path: PathBuf,
    pub modified_extensions: HashSet<Arc<str>>,
    pub wasm_host: Arc<WasmHost>,
    pub wasm_extensions: Vec<(Arc<ExtensionManifest>, WasmExtension)>,
    pub tasks: Vec<Task<()>>,
    pub(crate) remote_clients: HashMap<EntityId, RemoteClientState>,
    pub(crate) initial_index_load: Shared<Task<()>>,
    /// The latest version offered by each configured static registry, keyed by
    /// extension id. Refreshed on every registry fetch.
    pub registry_entries: HashMap<Arc<str>, RegistryEntry>,
    pub install_origins: InstallOrigins,
    pub origins_path: PathBuf,
    pub registry_cache_dir: PathBuf,
    pub git_checkouts_dir: PathBuf,
    /// Consecutive event-handler timeouts per extension.
    lathe_event_strikes: HashMap<Arc<str>, u32>,
    /// Extensions whose buffer events are paused. Survives the reloads that
    /// follow an interrupted handler; cleared when the extension is installed,
    /// upgraded, rebuilt or uninstalled.
    lathe_events_paused: HashSet<Arc<str>>,
    pub lathe_grants_path: PathBuf,
    /// Last seen `disabled_extensions`, to reload only when it changes.
    disabled_extensions: HashMap<Arc<str>, bool>,
    _settings_subscription: Option<Subscription>,
}

async fn load_lathe_grants(fs: &dyn Fs, path: &Path) -> wasm_host::lathe::LatheGrants {
    let Ok(content) = fs.load(path).await else {
        return Default::default();
    };
    serde_json::from_str(&content)
        .with_context(|| format!("parsing {path:?}"))
        .log_err()
        .unwrap_or_default()
}

/// How long an extension's buffer event handler may run before it counts as
/// a timeout.
pub const LATHE_EVENT_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive timeouts after which an extension's events are paused.
pub const LATHE_EVENT_MAX_STRIKES: u32 = 3;

pub(crate) struct RemoteClientState {
    dirty_tx: UnboundedSender<RemoteSyncSignal>,
    _task: Task<()>,
    _subscriptions: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteSyncSignal {
    IndexChanged,
    Reconnected,
}

#[derive(Clone, Copy)]
pub enum ExtensionOperation {
    Upgrade,
    Install,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionStatus {
    NotInstalled,
    Installing,
    Upgrading,
    Installed(Arc<str>),
    Removing,
}

#[derive(Clone)]
pub enum Event {
    ExtensionsUpdated,
    StartedReloading,
    ExtensionInstalled(Arc<str>),
    ExtensionUninstalled(Arc<str>),
    ExtensionFailedToLoad(Arc<str>),
}

impl EventEmitter<Event> for ExtensionStore {}

struct GlobalExtensionStore(Entity<ExtensionStore>);

impl Global for GlobalExtensionStore {}

#[derive(Debug, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ExtensionIndex {
    pub extensions: BTreeMap<Arc<str>, ExtensionIndexEntry>,
    pub themes: BTreeMap<Arc<str>, ExtensionIndexThemeEntry>,
    #[serde(default)]
    pub icon_themes: BTreeMap<Arc<str>, ExtensionIndexIconThemeEntry>,
    pub languages: BTreeMap<LanguageName, ExtensionIndexLanguageEntry>,
}

impl ExtensionIndex {
    fn extensions_to_sync_to_remote(&self) -> RemoteSyncExtensions {
        let mut extensions = RemoteSyncExtensions::default();

        for (id, entry) in &self.extensions {
            if !entry.disabled && entry.manifest.remote_load().is_some() {
                extensions.insert_extension_and_language_dependencies(self, id);
            }
        }

        extensions
    }
}

#[derive(Default)]
struct RemoteSyncExtensions(HashMap<Arc<str>, ExtensionIndexEntry>);

impl RemoteSyncExtensions {
    fn insert_extension_and_language_dependencies(
        &mut self,
        index: &ExtensionIndex,
        id: &Arc<str>,
    ) {
        if self.0.contains_key(id) {
            return;
        }

        let Some(entry) = index.extensions.get(id) else {
            return;
        };

        self.0.insert(id.clone(), entry.clone());

        let Some(remote_load) = entry.manifest.remote_load() else {
            return;
        };

        for language in remote_load.language_dependencies() {
            if let Some(language_entry) = index.languages.get(&language) {
                self.insert_extension_and_language_dependencies(index, &language_entry.extension);
            }
        }
    }

    fn into_entries(self) -> impl Iterator<Item = (Arc<str>, ExtensionIndexEntry)> {
        self.0.into_iter()
    }

    fn contains(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexEntry {
    pub manifest: Arc<ExtensionManifest>,
    pub dev: bool,
    /// Turned off in `disabled_extensions`: installed, but nothing it
    /// provides is loaded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexThemeEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexIconThemeEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexLanguageEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
    pub matcher: Arc<LanguageMatcher>,
    pub hidden: bool,
    pub grammar: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_files: Option<QueryFiles>,
}

actions!(
    zed,
    [
        /// Reloads all installed extensions.
        ReloadExtensions
    ]
);

pub fn init(
    extension_host_proxy: Arc<ExtensionHostProxy>,
    fs: Arc<dyn Fs>,
    client: Arc<Client>,
    node_runtime: NodeRuntime,
    cx: &mut App,
) {
    let store = cx.new(move |cx| {
        ExtensionStore::new(
            paths::extensions_dir().clone(),
            None,
            extension_host_proxy,
            fs,
            client.http_client(),
            client.http_client(),
            Some(client.telemetry().clone()),
            node_runtime,
            cx,
        )
    });

    cx.on_action(|_: &ReloadExtensions, cx| {
        let store = cx.global::<GlobalExtensionStore>().0.clone();
        store.update(cx, |store, cx| drop(store.reload(None, cx)));
    });

    cx.set_global(GlobalExtensionStore(store));

    cx.observe_new::<Project>(|project, _window, cx| {
        let Some(client) = project.remote_client() else {
            return;
        };
        if let Some(store) = ExtensionStore::try_global(cx) {
            store.update(cx, |store, cx| store.register_remote_client(client, cx));
        }
    })
    .detach();
}

impl ExtensionStore {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalExtensionStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalExtensionStore>().0.clone()
    }

    pub fn new(
        extensions_dir: PathBuf,
        build_dir: Option<PathBuf>,
        extension_host_proxy: Arc<ExtensionHostProxy>,
        fs: Arc<dyn Fs>,
        http_client: Arc<HttpClientWithUrl>,
        builder_client: Arc<dyn HttpClient>,
        telemetry: Option<Arc<Telemetry>>,
        node_runtime: NodeRuntime,
        cx: &mut Context<Self>,
    ) -> Self {
        let work_dir = extensions_dir.join("work");
        let build_dir = build_dir.unwrap_or_else(|| extensions_dir.join("build"));
        let installed_dir = extensions_dir.join("installed");
        let staging_dir = extensions_dir.join("staging");
        let index_path = extensions_dir.join("index.json");
        let origins_path = extensions_dir.join("origins.json");
        let lathe_grants_path = extensions_dir.join("lathe-grants.json");
        let registry_cache_dir = extensions_dir.join("registry-cache");
        let git_checkouts_dir = extensions_dir.join("git");

        let (reload_tx, mut reload_rx) = unbounded();
        let mut this = Self {
            proxy: extension_host_proxy.clone(),
            extension_index: Default::default(),
            installed_dir,
            staging_dir,
            index_path,
            builder: Arc::new(ExtensionBuilder::new(builder_client, build_dir)),
            outstanding_operations: Default::default(),
            modified_extensions: Default::default(),
            reload_complete_senders: Vec::new(),
            wasm_host: WasmHost::new(
                fs.clone(),
                http_client.clone(),
                node_runtime,
                extension_host_proxy,
                work_dir,
                cx,
            ),
            wasm_extensions: Vec::new(),
            fs,
            http_client,
            telemetry,
            reload_tx,
            tasks: Vec::new(),

            remote_clients: HashMap::default(),
            initial_index_load: Task::ready(()).shared(),
            registry_entries: HashMap::default(),
            install_origins: InstallOrigins::default(),
            origins_path,
            registry_cache_dir,
            git_checkouts_dir,
            lathe_event_strikes: HashMap::default(),
            lathe_events_paused: HashSet::default(),
            lathe_grants_path,
            disabled_extensions: HashMap::default(),
            _settings_subscription: None,
        };

        // The extensions store maintains an index file, which contains a complete
        // list of the installed extensions and the resources that they provide.
        // This index is loaded synchronously on startup.
        let (index_content, index_metadata, extensions_metadata, install_origins, lathe_grants) =
            cx.foreground_executor().block_on(async {
                futures::join!(
                    this.fs.load(&this.index_path),
                    this.fs.metadata(&this.index_path),
                    this.fs.metadata(&this.installed_dir),
                    InstallOrigins::load(this.fs.as_ref(), &this.origins_path),
                    load_lathe_grants(this.fs.as_ref(), &this.lathe_grants_path),
                )
            });
        this.install_origins = install_origins;
        this.wasm_host.set_lathe_grants(lathe_grants);

        // Normally, there is no need to rebuild the index. But if the index file
        // is invalid or is out-of-date according to the filesystem mtimes, then
        // it must be asynchronously rebuilt.
        let mut extension_index = ExtensionIndex::default();
        let mut extension_index_needs_rebuild = true;
        if let Ok(index_content) = index_content
            && let Some(index) = serde_json::from_str(&index_content).log_err()
        {
            extension_index = index;
            if let (Ok(Some(index_metadata)), Ok(Some(extensions_metadata))) =
                (index_metadata, extensions_metadata)
                && index_metadata
                    .mtime
                    .bad_is_greater_than(extensions_metadata.mtime)
            {
                extension_index_needs_rebuild = false;
            }
        }

        // Immediately load all of the extensions in the initial manifest. If the
        // index needs to be rebuild, then enqueue
        this.disabled_extensions = ExtensionSettings::get_global(cx)
            .disabled_extensions
            .clone();
        this._settings_subscription = Some(cx.observe_global::<SettingsStore>(|this, cx| {
            let disabled_extensions = ExtensionSettings::get_global(cx)
                .disabled_extensions
                .clone();
            if disabled_extensions != this.disabled_extensions {
                this.disabled_extensions = disabled_extensions;
                drop(this.reload(None, cx));
            }
        }));

        let load_initial_extensions = this.extensions_updated(extension_index, cx);
        let mut reload_future = None;
        if extension_index_needs_rebuild {
            reload_future = Some(this.reload(None, cx));
        }

        let initial_index_load = cx
            .spawn(async move |_, _| {
                if let Some(future) = reload_future {
                    future.await;
                }
            })
            .shared();
        this.initial_index_load = initial_index_load.clone();

        cx.spawn(async move |this, cx| {
            initial_index_load.await;
            this.update(cx, |this, cx| this.auto_install_extensions(cx))
                .ok();
            this.update(cx, |this, cx| this.check_for_updates(cx)).ok();
        })
        .detach();

        // Perform all extension loading in a single task to ensure that we
        // never attempt to simultaneously load/unload extensions from multiple
        // parallel tasks.
        this.tasks.push(cx.spawn(async move |this, cx| {
            async move {
                load_initial_extensions.await;

                let mut index_changed = false;
                let mut debounce_timer = cx.background_spawn(futures::future::pending()).fuse();

                loop {
                    select_biased! {
                        _ = debounce_timer => {
                            if index_changed {
                                let index = this
                                    .update(cx, |this, cx| this.rebuild_extension_index(cx))?
                                    .await;
                                this.update(cx, |this, cx| this.extensions_updated(index, cx))?
                                    .await;
                                index_changed = false;
                            }
                        }
                        extension_id = reload_rx.next() => {
                            let Some(extension_id) = extension_id else { break; };
                            this.update(cx, |this, _cx| {
                                this.modified_extensions.extend(extension_id);
                            })?;
                            index_changed = true;
                            debounce_timer = cx.background_executor().timer(RELOAD_DEBOUNCE_DURATION).fuse()
                        }
                    }
                }

                anyhow::Ok(())
            }
            .map(drop)
            .await;
        }));

        // Watch the installed extensions directory for changes. Whenever changes are
        // detected, rebuild the extension index, and load/unload any extensions that
        // have been added, removed, or modified.
        this.tasks.push(cx.background_spawn({
            let fs = this.fs.clone();
            let reload_tx = this.reload_tx.clone();
            let installed_dir = this.installed_dir.clone();
            async move {
                let (mut paths, _) = fs.watch(&installed_dir, FS_WATCH_LATENCY).await;
                while let Some(events) = paths.next().await {
                    for event in events {
                        let Ok(event_path) = event.path.strip_prefix(&installed_dir) else {
                            continue;
                        };

                        if let Some(path::Component::Normal(extension_dir_name)) =
                            event_path.components().next()
                            && let Some(extension_id) = extension_dir_name.to_str()
                        {
                            reload_tx.unbounded_send(Some(extension_id.into())).ok();
                        }
                    }
                }
            }
        }));

        this
    }

    pub fn reload(
        &mut self,
        modified_extension: Option<Arc<str>>,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let (tx, rx) = oneshot::channel();
        self.reload_complete_senders.push(tx);
        self.reload_tx
            .unbounded_send(modified_extension)
            .expect("reload task exited");
        cx.emit(Event::StartedReloading);

        async move {
            rx.await.ok();
        }
    }

    fn extensions_dir(&self) -> PathBuf {
        self.installed_dir.clone()
    }

    pub fn outstanding_operations(&self) -> &BTreeMap<Arc<str>, ExtensionOperation> {
        &self.outstanding_operations
    }

    pub fn installed_extensions(&self) -> &BTreeMap<Arc<str>, ExtensionIndexEntry> {
        &self.extension_index.extensions
    }

    /// Returns the status of the extension, where outstanding operations take
    /// precedence over the installed state.
    pub fn extension_status(&self, extension_id: &str) -> ExtensionStatus {
        match self.outstanding_operations.get(extension_id) {
            Some(ExtensionOperation::Install) => ExtensionStatus::Installing,
            Some(ExtensionOperation::Remove) => ExtensionStatus::Removing,
            Some(ExtensionOperation::Upgrade) => ExtensionStatus::Upgrading,
            None => match self.extension_index.extensions.get(extension_id) {
                Some(extension) => ExtensionStatus::Installed(extension.manifest.version.clone()),
                None => ExtensionStatus::NotInstalled,
            },
        }
    }

    /// Whether the extension is installed but turned off.
    pub fn is_extension_disabled(&self, extension_id: &str) -> bool {
        self.extension_index
            .extensions
            .get(extension_id)
            .is_some_and(|extension| extension.disabled)
    }

    pub fn is_dev_extension(&self, extension_id: &str) -> bool {
        self.extension_index
            .extensions
            .get(extension_id)
            .is_some_and(|extension| extension.dev)
    }

    pub fn dev_extensions(&self) -> impl Iterator<Item = &Arc<ExtensionManifest>> {
        self.extension_index
            .extensions
            .values()
            .filter_map(|extension| extension.dev.then_some(&extension.manifest))
    }

    pub fn extension_manifest_for_id(&self, extension_id: &str) -> Option<&Arc<ExtensionManifest>> {
        self.extension_index
            .extensions
            .get(extension_id)
            .map(|extension| &extension.manifest)
    }

    /// Returns the names of themes provided by extensions.
    pub fn extension_themes<'a>(
        &'a self,
        extension_id: &'a str,
    ) -> impl Iterator<Item = &'a Arc<str>> {
        self.extension_index
            .themes
            .iter()
            .filter_map(|(name, theme)| theme.extension.as_ref().eq(extension_id).then_some(name))
    }

    /// Returns the path to the theme file within an extension, if there is an
    /// extension that provides the theme.
    pub fn path_to_extension_theme(&self, theme_name: &str) -> Option<PathBuf> {
        let entry = self.extension_index.themes.get(theme_name)?;

        Some(
            self.extensions_dir()
                .join(entry.extension.as_ref())
                .join(&entry.path),
        )
    }

    /// Returns the names of icon themes provided by extensions.
    pub fn extension_icon_themes<'a>(
        &'a self,
        extension_id: &'a str,
    ) -> impl Iterator<Item = &'a Arc<str>> {
        self.extension_index
            .icon_themes
            .iter()
            .filter_map(|(name, icon_theme)| {
                icon_theme
                    .extension
                    .as_ref()
                    .eq(extension_id)
                    .then_some(name)
            })
    }

    /// Returns the path to the icon theme file within an extension, if there is
    /// an extension that provides the icon theme.
    pub fn path_to_extension_icon_theme(
        &self,
        icon_theme_name: &str,
    ) -> Option<(PathBuf, PathBuf)> {
        let entry = self.extension_index.icon_themes.get(icon_theme_name)?;

        let icon_theme_path = self
            .extensions_dir()
            .join(entry.extension.as_ref())
            .join(&entry.path);
        let icons_root_path = self.extensions_dir().join(entry.extension.as_ref());

        Some((icon_theme_path, icons_root_path))
    }

    pub fn fetch_extensions(
        &self,
        search: Option<&str>,
        provides_filter: Option<&BTreeSet<ExtensionProvides>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let version = SchemaVersion::CURRENT.to_string();
        let mut query = vec![("max_schema_version", version.as_str())];
        if let Some(search) = search {
            query.push(("filter", search));
        }

        let provides_filter = provides_filter.map(|provides_filter| {
            provides_filter
                .iter()
                .map(|provides| provides.to_string())
                .collect::<Vec<_>>()
                .join(",")
        });
        if let Some(provides_filter) = provides_filter.as_deref() {
            query.push(("provides", provides_filter));
        }

        let zed_task = self.fetch_extensions_from_api("/extensions", &query, cx);
        let registry_task = self.fetch_registry_entries(cx);
        let search = search.map(ToOwned::to_owned);
        let provides_filter = provides_filter_set(provides_filter.as_deref());
        let release_channel = ReleaseChannel::global(cx);
        cx.background_spawn(async move {
            let (zed_result, registry_entries) = futures::join!(zed_task, registry_task);
            let registry_extensions = registry_entries
                .values()
                .filter(|registry_entry| {
                    registry_entry
                        .entry
                        .matches(search.as_deref(), provides_filter.as_ref())
                })
                .map(|registry_entry| registry_entry.entry.to_metadata())
                .filter(|metadata| is_version_compatible(release_channel, metadata))
                .collect::<Vec<_>>();

            let zed_extensions = match zed_result {
                Ok(zed_extensions) => zed_extensions,
                Err(error) if !registry_extensions.is_empty() => {
                    log::error!("failed to fetch extensions from the Zed API: {error:#}");
                    Vec::new()
                }
                Err(error) => return Err(error),
            };

            Ok(registry::merge_extension_lists(
                zed_extensions,
                registry_extensions,
            ))
        })
    }

    /// Fetches every configured static registry and refreshes
    /// [`Self::registry_entries`]. Registries that fail to load are logged and skipped.
    pub fn fetch_registry_entries(
        &self,
        cx: &mut Context<Self>,
    ) -> Task<HashMap<Arc<str>, RegistryEntry>> {
        let registry_urls = ExtensionSettings::get_global(cx)
            .extension_registries
            .clone();
        if registry_urls.is_empty() {
            return Task::ready(HashMap::default());
        }

        let http_client = self.http_client.clone();
        let fs = self.fs.clone();
        let cache_dir = self.registry_cache_dir.clone();
        cx.spawn(async move |this, cx| {
            let indexes = join_all(registry_urls.into_iter().map(|url| {
                let http_client = http_client.clone();
                let fs = fs.clone();
                let cache_dir = cache_dir.clone();
                async move {
                    let index = registry::fetch_static_index(
                        http_client.as_ref(),
                        fs.as_ref(),
                        &cache_dir,
                        &url,
                    )
                    .await;
                    (url, index)
                }
            }))
            .await
            .into_iter()
            .filter_map(|(url, index)| match index {
                Ok(index) => Some((url, index)),
                Err(error) => {
                    log::error!("{error:#}");
                    None
                }
            });

            let mut entries = registry::collect_registry_entries(indexes);
            entries.retain(|id, _| !SUPPRESSED_EXTENSIONS.contains(id.as_ref()));

            this.update(cx, |this, _| this.registry_entries = entries.clone())
                .log_err();
            entries
        })
    }

    pub fn fetch_extensions_with_update_available(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let schema_versions = schema_version_range();
        let wasm_api_versions = wasm_api_version_range(ReleaseChannel::global(cx));
        let extension_settings = ExtensionSettings::get_global(cx);
        let updatable_ids = self
            .extension_index
            .extensions
            .iter()
            .filter(|(id, entry)| !entry.dev && extension_settings.should_auto_update(id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let extension_ids = updatable_ids
            .iter()
            .filter(|id| self.install_origins.get(id) == InstallOrigin::ZedApi)
            .map(|id| id.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        let registry_task = self.fetch_registry_entries(cx);
        let release_channel = ReleaseChannel::global(cx);
        let task = self.fetch_extensions_from_api(
            "/extensions/updates",
            &[
                ("min_schema_version", &schema_versions.start().to_string()),
                ("max_schema_version", &schema_versions.end().to_string()),
                (
                    "min_wasm_api_version",
                    &wasm_api_versions.start().to_string(),
                ),
                ("max_wasm_api_version", &wasm_api_versions.end().to_string()),
                ("ids", &extension_ids),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let (zed_result, registry_entries) = futures::join!(task, registry_task);
            let mut extensions = zed_result?;
            this.update(cx, |this, _cx| {
                // Extensions installed from a registry only update from that same
                // registry. Extensions installed from an archive URL never auto-update.
                extensions.extend(updatable_ids.iter().filter_map(|id| {
                    let InstallOrigin::Registry { index_url } = this.install_origins.get(id) else {
                        return None;
                    };
                    let registry_entry = registry_entries.get(id)?;
                    if registry_entry.index_url != index_url {
                        return None;
                    }
                    let metadata = registry_entry.entry.to_metadata();
                    is_version_compatible(release_channel, &metadata).then_some(metadata)
                }));

                extensions
                    .into_iter()
                    .filter(|extension| {
                        this.extension_index
                            .extensions
                            .get(&extension.id)
                            .is_none_or(|installed_extension| {
                                installed_extension.manifest.version != extension.manifest.version
                            })
                    })
                    .collect()
            })
        })
    }

    pub fn fetch_extension_versions(
        &self,
        extension_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let zed_task =
            self.fetch_extensions_from_api(&format!("/extensions/{extension_id}"), &[], cx);
        let registry_task = self.fetch_registry_entries(cx);
        let extension_id: Arc<str> = extension_id.into();
        cx.background_spawn(async move {
            // Static registries only publish the latest version of each extension.
            if let Some(registry_entry) = registry_task.await.get(&extension_id) {
                return Ok(vec![registry_entry.entry.to_metadata()]);
            }
            zed_task.await
        })
    }

    /// Installs any extensions that should be included with Zed by default.
    ///
    /// This can be used to make certain functionality provided by extensions
    /// available out-of-the-box.
    pub fn auto_install_extensions(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }

        let extension_settings = ExtensionSettings::get_global(cx);

        let extensions_to_install = extension_settings
            .auto_install_extensions
            .keys()
            .filter(|extension_id| extension_settings.should_auto_install(extension_id))
            .filter(|extension_id| {
                let is_already_installed = self
                    .extension_index
                    .extensions
                    .contains_key(extension_id.as_ref());
                !is_already_installed && !SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref())
            })
            .cloned()
            .collect::<Vec<_>>();

        cx.spawn(async move |this, cx| {
            for extension_id in extensions_to_install {
                this.update(cx, |this, cx| {
                    this.install_latest_extension(extension_id.clone(), cx);
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let task = self.fetch_extensions_with_update_available(cx);
        cx.spawn(async move |this, cx| Self::upgrade_extensions(this, task.await?, cx).await)
            .detach();
    }

    async fn upgrade_extensions(
        this: WeakEntity<Self>,
        extensions: Vec<ExtensionMetadata>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        for extension in extensions {
            let task = this.update(cx, |this, cx| {
                if let Some(installed_extension) =
                    this.extension_index.extensions.get(&extension.id)
                {
                    let installed_version =
                        Version::from_str(&installed_extension.manifest.version).ok()?;
                    let latest_version = Version::from_str(&extension.manifest.version).ok()?;

                    if installed_version >= latest_version {
                        return None;
                    }
                }

                Some(this.upgrade_extension(extension.id, extension.manifest.version, cx))
            })?;

            if let Some(task) = task {
                task.await.log_err();
            }
        }
        anyhow::Ok(())
    }

    fn fetch_extensions_from_api(
        &self,
        path: &str,
        query: &[(&str, &str)],
        cx: &mut Context<ExtensionStore>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let url = self.http_client.build_zed_api_url(path, query);
        let http_client = self.http_client.clone();
        cx.spawn(async move |_, _| {
            let mut response = http_client
                .get(url?.as_ref(), AsyncBody::empty(), true)
                .await?;

            let mut body = Vec::new();
            response
                .body_mut()
                .read_to_end(&mut body)
                .await
                .context("error reading extensions")?;

            if response.status().is_client_error() {
                let text = String::from_utf8_lossy(body.as_slice());
                bail!(
                    "status error {}, response: {text:?}",
                    response.status().as_u16()
                );
            }

            let mut response: GetExtensionsResponse = serde_json::from_slice(&body)?;

            response
                .data
                .retain(|extension| !SUPPRESSED_EXTENSIONS.contains(extension.id.as_ref()));

            Ok(response.data)
        })
    }

    pub fn install_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        self.install_or_upgrade_extension(extension_id, version, ExtensionOperation::Install, cx)
            .detach_and_log_err(cx);
    }

    fn install_or_upgrade_extension_at_endpoint(
        &mut self,
        extension_id: Arc<str>,
        url: Url,
        expected_sha256: Option<Arc<str>>,
        origin: InstallOrigin,
        requires: Vec<Arc<str>>,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extension_dir = self.installed_dir.join(extension_id.as_ref());
        let staging_dir = self.staging_dir.clone();
        let http_client = self.http_client.clone();
        let fs = self.fs.clone();

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return Task::ready(Ok(())),
            btree_map::Entry::Vacant(e) => e.insert(operation),
        };
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _finish = cx.on_drop(&this, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            cx.background_spawn(async move {
                let tar_gz_bytes = registry::download_archive(
                    http_client.as_ref(),
                    url.as_ref(),
                    expected_sha256.as_deref(),
                )
                .await?;

                let decompressed_bytes = GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice()));
                let archive = Archive::new(decompressed_bytes);

                let remove_dir = || {
                    fs.remove_dir(
                        &extension_dir,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                };

                let temp_dir = fs
                    .create_dir(&staging_dir)
                    .await
                    .and_then(|()| tempfile::tempdir_in(&staging_dir).map_err(Into::into));

                match temp_dir {
                    Ok(temp_dir) => {
                        archive.unpack(temp_dir.path()).await?;
                        remove_dir().await?;
                        fs.rename(
                            temp_dir.path(),
                            &extension_dir,
                            RenameOptions {
                                overwrite: true,
                                ignore_if_exists: true,
                                create_parents: true,
                            },
                        )
                        .await
                    }
                    Err(_) => {
                        remove_dir().await?;
                        archive.unpack(extension_dir).await.map_err(Into::into)
                    }
                }
            })
            .await?;

            this.update(cx, |this, cx| {
                this.lathe_events_paused.remove(&extension_id);
                this.record_install_origin(extension_id.clone(), origin, requires, cx)
            })?
            .await
            .log_err();

            this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
                .await;

            if let ExtensionOperation::Install = operation {
                this.update(cx, |this, cx| {
                    cx.emit(Event::ExtensionInstalled(extension_id.clone()));
                    if let Some(events) = ExtensionEvents::try_global(cx)
                        && let Some(manifest) = this.extension_manifest_for_id(&extension_id)
                    {
                        events.update(cx, |this, cx| {
                            this.emit(extension::Event::ExtensionInstalled(manifest.clone()), cx)
                        });
                    }
                })
                .ok();
            }

            anyhow::Ok(())
        })
    }

    pub fn install_latest_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
        log::info!("installing extension {extension_id} latest version");

        if let Some(registry_entry) = self.registry_entries.get(&extension_id).cloned() {
            self.install_from_registry_entry(registry_entry, ExtensionOperation::Install, cx)
                .detach_and_log_err(cx);
            return;
        }

        let schema_versions = schema_version_range();
        let wasm_api_versions = wasm_api_version_range(ReleaseChannel::global(cx));

        let Some(url) = self
            .http_client
            .build_zed_api_url(
                &format!("/extensions/{extension_id}/download"),
                &[
                    ("min_schema_version", &schema_versions.start().to_string()),
                    ("max_schema_version", &schema_versions.end().to_string()),
                    (
                        "min_wasm_api_version",
                        &wasm_api_versions.start().to_string(),
                    ),
                    ("max_wasm_api_version", &wasm_api_versions.end().to_string()),
                ],
            )
            .log_err()
        else {
            return;
        };

        self.install_or_upgrade_extension_at_endpoint(
            extension_id,
            url,
            None,
            InstallOrigin::ZedApi,
            Vec::new(),
            ExtensionOperation::Install,
            cx,
        )
        .detach_and_log_err(cx);
    }

    pub fn upgrade_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.install_or_upgrade_extension(extension_id, version, ExtensionOperation::Upgrade, cx)
    }

    fn install_or_upgrade_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        log::info!("installing extension {extension_id} {version}");

        if let Some(registry_entry) = self.registry_entries.get(&extension_id)
            && registry_entry.entry.version == version
        {
            let registry_entry = registry_entry.clone();
            return self.install_from_registry_entry(registry_entry, operation, cx);
        }

        let Some(url) = self
            .http_client
            .build_zed_api_url(
                &format!("/extensions/{extension_id}/{version}/download"),
                &[],
            )
            .log_err()
        else {
            return Task::ready(Ok(()));
        };

        self.install_or_upgrade_extension_at_endpoint(
            extension_id,
            url,
            None,
            InstallOrigin::ZedApi,
            Vec::new(),
            operation,
            cx,
        )
    }

    fn install_from_registry_entry(
        &mut self,
        registry_entry: RegistryEntry,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let RegistryEntry { index_url, entry } = registry_entry;
        log::info!(
            "installing extension {} {} from registry {index_url}",
            entry.id,
            entry.version
        );
        let url = match Url::parse(&entry.download_url) {
            Ok(url) => url,
            Err(error) => {
                return Task::ready(Err(anyhow!(
                    "invalid download URL {:?} for extension {}: {error}",
                    entry.download_url,
                    entry.id
                )));
            }
        };
        self.install_or_upgrade_extension_at_endpoint(
            entry.id,
            url,
            Some(entry.sha256),
            InstallOrigin::Registry { index_url },
            entry.requires,
            operation,
            cx,
        )
    }

    /// Commands registered by loaded extensions through the Lathe API, as
    /// `(extension manifest, command)` pairs.
    pub fn lathe_commands(
        &self,
    ) -> impl Iterator<Item = (&Arc<ExtensionManifest>, &wasm_host::lathe::LatheCommand)> {
        self.wasm_extensions
            .iter()
            .flat_map(|(manifest, extension)| {
                extension
                    .lathe_commands
                    .iter()
                    .map(move |command| (manifest, command))
            })
    }

    /// Panels registered by loaded extensions the user allows `ui:views`, as
    /// `(extension manifest, panel)` pairs.
    pub fn lathe_panels(
        &self,
    ) -> impl Iterator<Item = (&Arc<ExtensionManifest>, &wasm_host::lathe::LathePanel)> {
        self.wasm_extensions
            .iter()
            .filter(|(_, extension)| extension.may_show_views())
            .flat_map(|(manifest, extension)| {
                extension
                    .lathe_panels
                    .iter()
                    .map(move |panel| (manifest, panel))
            })
    }

    /// Previews registered by loaded extensions the user allows `ui:views`,
    /// as `(extension manifest, preview)` pairs.
    pub fn lathe_previews(
        &self,
    ) -> impl Iterator<Item = (&Arc<ExtensionManifest>, &wasm_host::lathe::LathePreview)> {
        self.wasm_extensions
            .iter()
            .filter(|(_, extension)| extension.may_show_views())
            .flat_map(|(manifest, extension)| {
                extension
                    .lathe_previews
                    .iter()
                    .map(move |preview| (manifest, preview))
            })
    }

    /// Tells an extension where its UI appears. See
    /// [`wasm_host::lathe::LatheSurfaceEvent`].
    pub fn dispatch_lathe_surface_event(
        &self,
        extension_id: &str,
        event: wasm_host::lathe::LatheSurfaceEvent,
        buffer: Option<Arc<dyn wasm_host::lathe::LatheEditorDelegate>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(extension) = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| extension.clone())
        else {
            return Task::ready(Err(anyhow!(
                "extension {extension_id} is not installed or failed to load"
            )));
        };
        cx.background_spawn(
            async move { extension.dispatch_lathe_surface_event(event, buffer).await },
        )
    }

    /// Asks an extension for rows `start..start + count` of a virtual list.
    pub fn lathe_provide_rows(
        &self,
        extension_id: &str,
        view_id: Arc<str>,
        list_id: Arc<str>,
        start: u32,
        count: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<wasm_host::lathe_views::NodeKind>>> {
        let Some(extension) = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| extension.clone())
        else {
            return Task::ready(Err(anyhow!(
                "extension {extension_id} is not installed or failed to load"
            )));
        };
        cx.background_spawn(async move {
            extension
                .lathe_provide_rows(view_id, list_id, start, count)
                .await
        })
    }

    /// Delivers a click on an element of one of an extension's views.
    pub fn dispatch_lathe_view_event(
        &self,
        extension_id: &str,
        view_id: Arc<str>,
        element_id: Arc<str>,
        event: wasm_host::lathe_views::ViewEvent,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(extension) = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| extension.clone())
        else {
            return Task::ready(Err(anyhow!(
                "extension {extension_id} is not installed or failed to load"
            )));
        };
        cx.background_spawn(async move {
            extension
                .dispatch_lathe_view_event(view_id, element_id, event)
                .await
        })
    }

    /// Runs a command that an extension registered through the Lathe API.
    pub fn run_lathe_command(
        &self,
        extension_id: &str,
        command_id: Arc<str>,
        editor: Option<Arc<dyn wasm_host::lathe::LatheEditorDelegate>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(extension) = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| extension.clone())
        else {
            return Task::ready(Err(anyhow!(
                "extension {extension_id} is not installed or failed to load"
            )));
        };
        cx.background_spawn(async move { extension.run_lathe_command(command_id, editor).await })
    }

    /// Delivers a buffer event to every loaded extension that declared the
    /// matching capability. Each handler gets [`LATHE_EVENT_TIMEOUT`]; after
    /// [`LATHE_EVENT_MAX_STRIKES`] timeouts in a row, that extension's events
    /// are paused until it is reloaded. Resolves to the extensions this
    /// event caused to be paused, as `(id, name)` pairs.
    pub fn dispatch_lathe_buffer_event(
        &mut self,
        event: wasm_host::lathe::LatheBufferEvent,
        buffer: Arc<dyn wasm_host::lathe::LatheEditorDelegate>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<(Arc<str>, Arc<str>)>> {
        let capability = event.capability();
        let targets = self
            .wasm_extensions
            .iter()
            .filter(|(manifest, extension)| {
                extension.lathe_capabilities.contains(&capability)
                    && !self.lathe_events_paused.contains(&manifest.id)
            })
            .map(|(manifest, extension)| (manifest.id.clone(), extension.clone()))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Task::ready(Vec::new());
        }
        let interrupts = targets
            .iter()
            .map(|(extension_id, extension)| {
                (extension_id.clone(), extension.lathe_interrupt.clone())
            })
            .collect::<HashMap<_, _>>();

        let executor = cx.background_executor().clone();
        let deliveries = targets
            .into_iter()
            .map(|(extension_id, extension)| {
                let buffer = buffer.clone();
                let timeout = executor.timer(LATHE_EVENT_TIMEOUT);
                async move {
                    let delivery = extension.dispatch_lathe_buffer_event(event, buffer);
                    futures::pin_mut!(delivery);
                    let outcome = match futures::future::select(delivery, timeout).await {
                        futures::future::Either::Left((result, _)) => Some(result),
                        futures::future::Either::Right(_) => None,
                    };
                    (extension_id, outcome)
                }
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |this, cx| {
            let outcomes = join_all(deliveries).await;
            this.update(cx, |this, cx| {
                outcomes
                    .into_iter()
                    .filter_map(|(extension_id, outcome)| {
                        if outcome.is_none() {
                            // Stop the stuck handler. A trapped instance cannot be
                            // called again, so load the extension afresh.
                            if let Some(interrupt) = interrupts.get(&extension_id) {
                                interrupt.interrupt();
                            }
                            drop(this.reload(Some(extension_id.clone()), cx));
                        }
                        this.record_lathe_event_outcome(extension_id, event, outcome, cx)
                    })
                    .collect()
            })
            .log_err()
            .unwrap_or_default()
        })
    }

    fn record_lathe_event_outcome(
        &mut self,
        extension_id: Arc<str>,
        event: wasm_host::lathe::LatheBufferEvent,
        outcome: Option<Result<()>>,
        cx: &mut Context<Self>,
    ) -> Option<(Arc<str>, Arc<str>)> {
        match outcome {
            Some(result) => {
                self.lathe_event_strikes.remove(&extension_id);
                if let Err(error) = result {
                    log::error!("extension {extension_id} failed handling {event:?}: {error:#}");
                }
                None
            }
            None => {
                let strikes = self
                    .lathe_event_strikes
                    .entry(extension_id.clone())
                    .or_default();
                *strikes += 1;
                log::warn!(
                    "extension {extension_id} took longer than {LATHE_EVENT_TIMEOUT:?} handling {event:?} ({strikes} in a row)"
                );
                if *strikes < LATHE_EVENT_MAX_STRIKES {
                    return None;
                }
                log::error!("pausing buffer events for extension {extension_id}");
                self.lathe_event_strikes.remove(&extension_id);
                self.lathe_events_paused.insert(extension_id.clone());
                cx.notify();
                let name = self
                    .extension_manifest_for_id(&extension_id)
                    .map(|manifest| Arc::<str>::from(manifest.name.as_str()))
                    .unwrap_or_else(|| extension_id.clone());
                Some((extension_id, name))
            }
        }
    }

    /// Lathe capabilities a loaded extension declares that the user has not
    /// granted yet.
    pub fn lathe_capabilities_needing_consent(
        &self,
        extension_id: &str,
    ) -> Vec<wasm_host::lathe::LatheCapability> {
        self.wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| {
                extension
                    .lathe_declared_capabilities
                    .difference(&extension.lathe_capabilities)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Lathe capabilities the user has granted an extension.
    pub fn granted_lathe_capabilities(
        &self,
        extension_id: &str,
    ) -> collections::BTreeSet<wasm_host::lathe::LatheCapability> {
        self.wasm_host.lathe_grants().granted(extension_id)
    }

    /// Grants capabilities and reloads the extension so they take effect.
    /// Granting before installing is allowed; the grant applies on first load.
    pub fn grant_lathe_capabilities(
        &mut self,
        extension_id: Arc<str>,
        capabilities: Vec<wasm_host::lathe::LatheCapability>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let save = self.update_lathe_grants(cx, |grants| {
            grants.grant(extension_id.clone(), capabilities)
        });
        self.reload_after_grant_change(extension_id, save, cx)
    }

    /// Withdraws every Lathe capability granted to the extension and reloads it.
    pub fn revoke_lathe_capabilities(
        &mut self,
        extension_id: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let save = self.update_lathe_grants(cx, |grants| grants.revoke(&extension_id));
        self.reload_after_grant_change(extension_id, save, cx)
    }

    fn reload_after_grant_change(
        &mut self,
        extension_id: Arc<str>,
        save: Task<Result<()>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |this, cx| {
            save.await?;
            let is_installed = this.read_with(cx, |this, _| {
                this.extension_index.extensions.contains_key(&extension_id)
            })?;
            if is_installed {
                this.update(cx, |this, cx| this.reload(Some(extension_id), cx))?
                    .await;
            }
            Ok(())
        })
    }

    fn update_lathe_grants(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut wasm_host::lathe::LatheGrants),
    ) -> Task<Result<()>> {
        let mut grants = self.wasm_host.lathe_grants();
        update(&mut grants);
        self.wasm_host.set_lathe_grants(grants.clone());
        cx.notify();
        let fs = self.fs.clone();
        let path = self.lathe_grants_path.clone();
        cx.background_spawn(async move {
            let content = serde_json::to_string_pretty(&grants)?;
            fs.save(&path, &content.as_str().into(), Default::default())
                .await
                .with_context(|| format!("saving {path:?}"))
        })
    }

    /// Whether an extension's buffer events were paused for repeated timeouts.
    pub fn are_lathe_events_paused(&self, extension_id: &str) -> bool {
        self.lathe_events_paused.contains(extension_id)
    }

    /// Lathe API capabilities of an extension, from the loaded extension if it
    /// is installed, otherwise from the registry offering it.
    pub fn lathe_capabilities_for_display(
        &self,
        extension_id: &str,
    ) -> Vec<wasm_host::lathe::LatheCapability> {
        if let Some((_, extension)) = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            && !extension.lathe_declared_capabilities.is_empty()
        {
            return extension
                .lathe_declared_capabilities
                .iter()
                .copied()
                .collect();
        }
        self.registry_entries
            .get(extension_id)
            .and_then(|registry_entry| {
                wasm_host::lathe::LatheCapability::parse_all(
                    &registry_entry.entry.lathe_capabilities,
                )
                .ok()
            })
            .map(|capabilities| capabilities.into_iter().collect())
            .unwrap_or_default()
    }

    /// Lathe API capabilities declared by a loaded extension.
    pub fn lathe_capabilities(
        &self,
        extension_id: &str,
    ) -> Option<Arc<collections::BTreeSet<wasm_host::lathe::LatheCapability>>> {
        self.wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .map(|(_, extension)| extension.lathe_capabilities.clone())
            .filter(|capabilities| !capabilities.is_empty())
    }

    /// Whether the extension is marked as only working in Lathe, either by the
    /// registry offering it or by the record kept when it was installed.
    ///
    /// For an installed extension this comes from the extension itself: its
    /// `requires` in `extension.toml`, or the fact that it uses the Lathe API
    /// (which makes it Lathe-only even if the author left `requires` out).
    pub fn is_lathe_only(&self, extension_id: &str) -> bool {
        let is_lathe_requirement =
            |requirement: &Arc<str>| requirement.as_ref() == registry::LATHE_REQUIREMENT;
        let loaded = self
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == extension_id)
            .is_some_and(|(_, extension)| {
                extension.lathe_api_version.is_some()
                    || extension.lathe_requires.iter().any(is_lathe_requirement)
            });
        loaded
            || self
                .registry_entries
                .get(extension_id)
                .is_some_and(|registry_entry| registry_entry.entry.is_lathe_only())
            || self
                .install_origins
                .requires(extension_id)
                .iter()
                .any(is_lathe_requirement)
    }

    fn record_install_origin(
        &mut self,
        extension_id: Arc<str>,
        origin: InstallOrigin,
        requires: Vec<Arc<str>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.install_origins.set(extension_id, origin, requires);
        self.save_install_origins(cx)
    }

    fn save_install_origins(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let fs = self.fs.clone();
        let origins = self.install_origins.clone();
        let origins_path = self.origins_path.clone();
        cx.background_spawn(async move { origins.save(fs.as_ref(), &origins_path).await })
    }

    /// Installs a packaged extension archive (`.tar.gz`, as produced by the
    /// extension CLI) from an arbitrary URL. The extension id is read from the
    /// archive's manifest. Extensions installed this way do not auto-update.
    pub fn install_extension_from_archive_url(
        &mut self,
        url: Url,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<str>>> {
        // Archives from a URL have no published checksum, so require TLS.
        if url.scheme() != "https" {
            return Task::ready(Err(anyhow!(
                "extension archive URL must use https, got {url}"
            )));
        }

        let staging_dir = self.staging_dir.clone();
        let installed_dir = self.installed_dir.clone();
        let http_client = self.http_client.clone();
        let fs = self.fs.clone();

        cx.spawn(async move |this, cx| {
            let (temp_dir, manifest, lathe_fields) = cx
                .background_spawn({
                    let url = url.clone();
                    let fs = fs.clone();
                    async move {
                        let tar_gz_bytes =
                            registry::download_archive(http_client.as_ref(), url.as_ref(), None)
                                .await?;
                        fs.create_dir(&staging_dir).await?;
                        let temp_dir = tempfile::tempdir_in(&staging_dir)?;
                        let decompressed_bytes =
                            GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice()));
                        Archive::new(decompressed_bytes)
                            .unpack(temp_dir.path())
                            .await
                            .with_context(|| format!("unpacking extension archive from {url}"))?;
                        let manifest = ExtensionManifest::load(fs.clone(), temp_dir.path())
                            .await
                            .with_context(|| format!("no valid extension manifest in {url}"))?;
                        let lathe_fields =
                            LatheManifestFields::load(fs.as_ref(), temp_dir.path()).await?;
                        anyhow::Ok((temp_dir, manifest, lathe_fields))
                    }
                })
                .await?;

            let extension_id = manifest.id.clone();
            if SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref()) {
                bail!("extension {extension_id} is no longer supported");
            }
            let unsupported = registry::unsupported_requirements(&lathe_fields.requires);
            if !unsupported.is_empty() {
                bail!(
                    "extension {extension_id} requires {}, which this version of Lathe does not support",
                    unsupported.join(", ")
                );
            }

            let operation = this.update(cx, |this, cx| {
                if this
                    .extension_index
                    .extensions
                    .get(&extension_id)
                    .is_some_and(|entry| entry.dev)
                {
                    bail!(
                        "extension {extension_id} is installed as a dev extension; uninstall it first"
                    );
                }
                let operation = if this.extension_index.extensions.contains_key(&extension_id) {
                    ExtensionOperation::Upgrade
                } else {
                    ExtensionOperation::Install
                };
                match this.outstanding_operations.entry(extension_id.clone()) {
                    btree_map::Entry::Occupied(_) => {
                        bail!("extension {extension_id} is already being installed or removed")
                    }
                    btree_map::Entry::Vacant(entry) => entry.insert(operation),
                };
                cx.notify();
                anyhow::Ok(operation)
            })??;

            let _finish = cx.on_drop(&this, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            let extension_dir = installed_dir.join(extension_id.as_ref());
            cx.background_spawn(async move {
                fs.remove_dir(
                    &extension_dir,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
                fs.rename(
                    temp_dir.path(),
                    &extension_dir,
                    RenameOptions {
                        overwrite: true,
                        ignore_if_exists: true,
                        create_parents: true,
                    },
                )
                .await
            })
            .await?;

            this.update(cx, |this, cx| {
                this.lathe_events_paused.remove(&extension_id);
                this.record_install_origin(
                    extension_id.clone(),
                    InstallOrigin::ArchiveUrl {
                        url: url.as_str().into(),
                    },
                    lathe_fields.requires,
                    cx,
                )
            })?
            .await
            .log_err();

            this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
                .await;

            if let ExtensionOperation::Install = operation {
                this.update(cx, |this, cx| {
                    cx.emit(Event::ExtensionInstalled(extension_id.clone()));
                    if let Some(events) = ExtensionEvents::try_global(cx)
                        && let Some(manifest) = this.extension_manifest_for_id(&extension_id)
                    {
                        events.update(cx, |this, cx| {
                            this.emit(extension::Event::ExtensionInstalled(manifest.clone()), cx)
                        });
                    }
                })
                .ok();
            }

            Ok(extension_id)
        })
    }

    pub fn uninstall_extension(
        &mut self,
        extension_id: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extension_dir = self.installed_dir.join(extension_id.as_ref());
        let work_dir = self.wasm_host.work_dir.join(extension_id.as_ref());
        let fs = self.fs.clone();

        let extension_manifest = self.extension_manifest_for_id(&extension_id).cloned();
        let git_checkout = match self.install_origins.get(&extension_id) {
            InstallOrigin::GitUrl { checkout, .. }
                if checkout.starts_with(&self.git_checkouts_dir) =>
            {
                Some(checkout)
            }
            _ => None,
        };

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return Task::ready(Ok(())),
            btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Remove),
        };

        cx.spawn(async move |extension_store, cx| {
            let _finish = cx.on_drop(&extension_store, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            fs.remove_dir(
                &extension_dir,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await
            .with_context(|| format!("Removing extension dir {extension_dir:?}"))?;

            extension_store
                .update(cx, |extension_store, cx| extension_store.reload(None, cx))?
                .await;

            // There's a race between wasm extension fully stopping and the directory removal.
            // On Windows, it's impossible to remove a directory that has a process running in it.
            for i in 0..3 {
                cx.background_executor()
                    .timer(Duration::from_millis(i * 100))
                    .await;
                let removal_result = fs
                    .remove_dir(
                        &work_dir,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await;
                match removal_result {
                    Ok(()) => break,
                    Err(e) => {
                        if i == 2 {
                            log::error!("Failed to remove extension work dir {work_dir:?} : {e}");
                        }
                    }
                }
            }

            if let Some(git_checkout) = git_checkout {
                fs.remove_dir(
                    &git_checkout,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .with_context(|| format!("removing git checkout {git_checkout:?}"))
                .log_err();
            }

            extension_store
                .update(cx, |extension_store, cx| {
                    extension_store.install_origins.remove(&extension_id);
                    extension_store.lathe_events_paused.remove(&extension_id);
                    extension_store.lathe_event_strikes.remove(&extension_id);
                    extension_store.save_install_origins(cx)
                })?
                .await
                .log_err();
            extension_store
                .update(cx, |extension_store, cx| {
                    extension_store.update_lathe_grants(cx, |grants| grants.revoke(&extension_id))
                })?
                .await
                .log_err();

            extension_store.update(cx, |_, cx| {
                cx.emit(Event::ExtensionUninstalled(extension_id.clone()));
                if let Some(events) = ExtensionEvents::try_global(cx)
                    && let Some(manifest) = extension_manifest
                {
                    events.update(cx, |this, cx| {
                        this.emit(extension::Event::ExtensionUninstalled(manifest.clone()), cx)
                    });
                }
            })?;

            anyhow::Ok(())
        })
    }

    /// Builds and installs an extension from a local directory. The user picked
    /// the directory themselves, so the Lathe capabilities it declares are
    /// granted.
    pub fn install_dev_extension(
        &mut self,
        extension_source_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.install_dev_extension_with_grants(extension_source_path, true, cx)
    }

    fn install_dev_extension_with_grants(
        &mut self,
        extension_source_path: PathBuf,
        grant_declared_capabilities: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extensions_dir = self.extensions_dir();
        let fs = self.fs.clone();
        let builder = self.builder.clone();

        cx.spawn(async move |this, cx| {
            let mut extension_manifest =
                ExtensionManifest::load(fs.clone(), &extension_source_path).await?;
            let extension_id = extension_manifest.id.clone();

            if let Some(uninstall_task) = this
                .update(cx, |this, cx| {
                    this.extension_index
                        .extensions
                        .get(extension_id.as_ref())
                        .is_some_and(|index_entry| !index_entry.dev)
                        .then(|| this.uninstall_extension(extension_id.clone(), cx))
                })
                .ok()
                .flatten()
            {
                uninstall_task.await.log_err();
            }

            if !this.update(cx, |this, cx| {
                match this.outstanding_operations.entry(extension_id.clone()) {
                    btree_map::Entry::Occupied(_) => return false,
                    btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Install),
                };
                cx.notify();
                true
            })? {
                return Ok(());
            }

            let _finish = cx.on_drop(&this, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            cx.background_spawn({
                let extension_source_path = extension_source_path.clone();
                let fs = fs.clone();
                async move {
                    builder
                        .compile_extension(
                            &extension_source_path,
                            &mut extension_manifest,
                            CompileExtensionOptions::dev(),
                            fs,
                        )
                        .await
                }
            })
            .await
            .inspect_err(|error| {
                util::log_err(error);
            })?;

            let output_path = &extensions_dir.join(extension_id.as_ref());
            if let Some(metadata) = fs.metadata(output_path).await? {
                if metadata.is_symlink {
                    fs.remove_file(
                        output_path,
                        RemoveOptions {
                            recursive: false,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await?;
                } else {
                    bail!("extension {extension_id} is still installed");
                }
            }

            fs.create_symlink(output_path, extension_source_path.clone())
                .await?;

            if grant_declared_capabilities {
                let lathe_fields =
                    LatheManifestFields::load(fs.as_ref(), &extension_source_path).await?;
                let declared =
                    wasm_host::lathe::LatheCapability::parse_all(&lathe_fields.lathe_capabilities)?;
                if !declared.is_empty() {
                    this.update(cx, |this, cx| {
                        this.update_lathe_grants(cx, |grants| {
                            grants.grant(extension_id.clone(), declared)
                        })
                    })?
                    .await
                    .log_err();
                }
            }
            this.update(cx, |this, _| this.lathe_events_paused.remove(&extension_id))?;

            this.update(cx, |this, cx| this.reload(None, cx))?.await;
            this.update(cx, |this, cx| {
                cx.emit(Event::ExtensionInstalled(extension_id.clone()));
                if let Some(events) = ExtensionEvents::try_global(cx)
                    && let Some(manifest) = this.extension_manifest_for_id(&extension_id)
                {
                    events.update(cx, |this, cx| {
                        this.emit(extension::Event::ExtensionInstalled(manifest.clone()), cx)
                    });
                }
            })?;

            Ok(())
        })
    }

    /// Clones a git repository and installs it as a dev extension, which builds
    /// it locally. Building requires the same toolchain as any dev extension
    /// (`rustup` with the `wasm32-wasip2` target for extensions with Rust code).
    /// Cloning the same URL again replaces the previous checkout.
    pub fn install_extension_from_git_url(
        &mut self,
        url: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<str>>> {
        let url = url.trim().to_string();
        if let Err(error) = registry::validate_git_url(&url) {
            return Task::ready(Err(error));
        }

        let checkout = self
            .git_checkouts_dir
            .join(registry::git_checkout_dir_name(&url));
        let git_checkouts_dir = self.git_checkouts_dir.clone();
        let fs = self.fs.clone();

        cx.spawn(async move |this, cx| {
            let lathe_fields = cx
                .background_spawn({
                    let url = url.clone();
                    let checkout = checkout.clone();
                    let fs = fs.clone();
                    async move {
                        which::which("git").context(
                            "installing an extension from a git URL requires git on your PATH",
                        )?;
                        fs.create_dir(&git_checkouts_dir).await?;
                        fs.remove_dir(
                            &checkout,
                            RemoveOptions {
                                recursive: true,
                                ignore_if_not_exists: true,
                            },
                        )
                        .await?;
                        let output = util::command::new_command("git")
                            .arg("clone")
                            .arg("--depth=1")
                            .arg("--recurse-submodules")
                            .arg("--shallow-submodules")
                            .arg("--")
                            .arg(&url)
                            .arg(&checkout)
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .output()
                            .await
                            .context("running git clone")?;
                        if !output.status.success() {
                            bail!(
                                "git clone {url} failed: {}",
                                String::from_utf8_lossy(&output.stderr).trim()
                            );
                        }
                        LatheManifestFields::load(fs.as_ref(), &checkout).await
                    }
                })
                .await?;

            let unsupported = registry::unsupported_requirements(&lathe_fields.requires);
            if !unsupported.is_empty() {
                bail!(
                    "the extension at {url} requires {}, which this version of Lathe does not support",
                    unsupported.join(", ")
                );
            }

            let manifest = ExtensionManifest::load(fs.clone(), &checkout)
                .await
                .with_context(|| format!("no valid extension manifest at the root of {url}"))?;
            let extension_id = manifest.id.clone();

            // Code from a URL is not the user's own, so its Lathe capabilities
            // wait for the user to allow them.
            this.update(cx, |this, cx| {
                this.install_dev_extension_with_grants(checkout.clone(), false, cx)
            })?
                .await?;

            this.update(cx, |this, cx| {
                this.record_install_origin(
                    extension_id.clone(),
                    InstallOrigin::GitUrl {
                        url: url.as_str().into(),
                        checkout,
                    },
                    lathe_fields.requires,
                    cx,
                )
            })?
            .await
            .log_err();

            Ok(extension_id)
        })
    }

    pub fn rebuild_dev_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
        self.lathe_events_paused.remove(&extension_id);
        let path = self.installed_dir.join(extension_id.as_ref());
        let builder = self.builder.clone();
        let fs = self.fs.clone();

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return,
            btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Upgrade),
        };

        cx.notify();
        let compile = cx.background_spawn(async move {
            let mut manifest = ExtensionManifest::load(fs.clone(), &path).await?;
            builder
                .compile_extension(&path, &mut manifest, CompileExtensionOptions::dev(), fs)
                .await
        });

        cx.spawn(async move |this, cx| {
            let result = compile.await;

            this.update(cx, |this, cx| {
                this.outstanding_operations.remove(&extension_id);
                cx.notify();
            })?;

            if result.is_ok() {
                this.update(cx, |this, cx| this.reload(Some(extension_id), cx))?
                    .await;
            }

            result
        })
        .detach_and_log_err(cx)
    }

    /// Updates the set of installed extensions.
    ///
    /// First, this unloads any themes, languages, or grammars that are
    /// no longer in the manifest, or whose files have changed on disk.
    /// Then it loads any themes, languages, or grammars that are newly
    /// added to the manifest, or whose files have changed on disk.
    #[ztracing::instrument(skip_all)]
    fn extensions_updated(
        &mut self,
        mut new_index: ExtensionIndex,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let old_index = &self.extension_index;

        let suppressed_extensions_to_remove = new_index
            .extensions
            .extract_if(.., |extension_id, _| {
                SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref())
            })
            .collect::<Vec<_>>();

        // Disabled extensions stay in the index, marked, so they still show as
        // installed. Flipping the mark changes the entry, which makes the diff
        // below unload it; it is then left out of `extensions_to_load`.
        let extension_settings = ExtensionSettings::get_global(cx);
        for (extension_id, entry) in new_index.extensions.iter_mut() {
            entry.disabled = extension_settings.is_disabled(extension_id);
        }
        let disabled = new_index
            .extensions
            .iter()
            .filter(|(_, entry)| entry.disabled)
            .map(|(extension_id, _)| extension_id.clone())
            .collect::<HashSet<_>>();
        new_index
            .languages
            .retain(|_, entry| !disabled.contains(&entry.extension));
        new_index
            .themes
            .retain(|_, entry| !disabled.contains(&entry.extension));
        new_index
            .icon_themes
            .retain(|_, entry| !disabled.contains(&entry.extension));

        // Determine which extensions need to be loaded and unloaded, based
        // on the changes to the manifest and the extensions that we know have been
        // modified.
        let mut extensions_to_unload = Vec::default();
        let mut extensions_to_load = Vec::default();
        {
            let mut old_keys = old_index.extensions.iter().peekable();
            let mut new_keys = new_index.extensions.iter().peekable();
            loop {
                match (old_keys.peek(), new_keys.peek()) {
                    (None, None) => break,
                    (None, Some(_)) => {
                        extensions_to_load.push(new_keys.next().unwrap().0.clone());
                    }
                    (Some(_), None) => {
                        extensions_to_unload.push(old_keys.next().unwrap().0.clone());
                    }
                    (Some((old_key, _)), Some((new_key, _))) => match old_key.cmp(new_key) {
                        Ordering::Equal => {
                            let (old_key, old_value) = old_keys.next().unwrap();
                            let (new_key, new_value) = new_keys.next().unwrap();
                            if old_value != new_value || self.modified_extensions.contains(old_key)
                            {
                                extensions_to_unload.push(old_key.clone());
                                extensions_to_load.push(new_key.clone());
                            }
                        }
                        Ordering::Less => {
                            extensions_to_unload.push(old_keys.next().unwrap().0.clone());
                        }
                        Ordering::Greater => {
                            extensions_to_load.push(new_keys.next().unwrap().0.clone());
                        }
                    },
                }
            }
            self.modified_extensions.clear();
        }
        extensions_to_load.retain(|extension_id| !disabled.contains(extension_id));

        let trigger_suppressed_extension_removal =
            move |this: &mut ExtensionStore, cx: &mut Context<ExtensionStore>| {
                for (id, _) in suppressed_extensions_to_remove {
                    this.uninstall_extension(id, cx).detach_and_log_err(cx);
                }
            };

        if extensions_to_load.is_empty() && extensions_to_unload.is_empty() {
            self.reload_complete_senders.clear();
            trigger_suppressed_extension_removal(self, cx);
            return Task::ready(());
        }

        let reload_count = extensions_to_unload
            .iter()
            .filter(|id| extensions_to_load.contains(id))
            .count();

        log::info!(
            "extensions updated. loading {}, reloading {}, unloading {}",
            extensions_to_load.len() - reload_count,
            reload_count,
            extensions_to_unload.len() - reload_count
        );

        let old_remote_sync_extensions = old_index.extensions_to_sync_to_remote();
        let new_remote_sync_extensions = new_index.extensions_to_sync_to_remote();
        let remote_sync_changed = extensions_to_unload
            .iter()
            .any(|id| old_remote_sync_extensions.contains(id.as_ref()))
            || extensions_to_load
                .iter()
                .any(|id| new_remote_sync_extensions.contains(id.as_ref()));

        let extension_ids = extensions_to_load
            .iter()
            .filter_map(|id| {
                Some((
                    id.clone(),
                    new_index.extensions.get(id)?.manifest.version.clone(),
                ))
            })
            .collect::<Vec<_>>();

        telemetry::event!("Extensions Loaded", id_and_versions = extension_ids);

        let themes_to_remove = old_index
            .themes
            .iter()
            .filter_map(|(name, entry)| {
                if extensions_to_unload.contains(&entry.extension) {
                    Some(name.clone().into())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let icon_themes_to_remove = old_index
            .icon_themes
            .iter()
            .filter_map(|(name, entry)| {
                if extensions_to_unload.contains(&entry.extension) {
                    Some(name.clone().into())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let mut languages_to_remove = Vec::new();
        let mut languages_to_readd = Vec::new();
        for (name, entry) in &old_index.languages {
            if !extensions_to_unload.contains(&entry.extension) {
                continue;
            }
            match new_index.languages.get(name) {
                Some(new_entry) if !extensions_to_load.contains(&new_entry.extension) => {
                    languages_to_readd.push((name.clone(), new_entry.clone()));
                }
                _ => languages_to_remove.push(name.clone()),
            }
        }
        let mut grammars_to_remove = Vec::new();
        let mut server_removal_tasks = Vec::with_capacity(extensions_to_unload.len());
        for extension_id in &extensions_to_unload {
            let Some(extension) = old_index.extensions.get(extension_id) else {
                continue;
            };
            grammars_to_remove.extend(extension.manifest.grammars.keys().cloned());
            for (language_server_name, config) in &extension.manifest.language_servers {
                for language in config.languages() {
                    server_removal_tasks.push(self.proxy.remove_language_server(
                        &language,
                        language_server_name,
                        cx,
                    ));
                }
            }

            for server_id in extension.manifest.context_servers.keys() {
                self.proxy.unregister_context_server(server_id.clone(), cx);
            }
            for adapter in extension.manifest.debug_adapters.keys() {
                self.proxy.unregister_debug_adapter(adapter.clone());
            }
            for locator in extension.manifest.debug_locators.keys() {
                self.proxy.unregister_debug_locator(locator.clone());
            }
        }

        self.wasm_extensions
            .retain(|(extension, _)| !extensions_to_unload.contains(&extension.id));

        self.proxy.remove_user_themes(themes_to_remove);
        self.proxy.remove_icon_themes(icon_themes_to_remove);
        self.proxy
            .update_languages(&[], &grammars_to_remove, Vec::new());

        let mut grammars_to_add = Vec::new();
        let mut themes_to_add = Vec::new();
        let mut icon_themes_to_add = Vec::new();
        let mut snippets_to_add = Vec::new();
        for extension_id in &extensions_to_load {
            let Some(extension) = new_index.extensions.get(extension_id) else {
                continue;
            };

            grammars_to_add.extend(extension.manifest.grammars.keys().map(|grammar_name| {
                let mut grammar_path = self.installed_dir.clone();
                grammar_path.extend([extension_id.as_ref(), "grammars"]);
                grammar_path.push(grammar_name.as_ref());
                grammar_path.set_extension("wasm");
                (grammar_name.clone(), grammar_path)
            }));
            themes_to_add.extend(extension.manifest.themes.iter().map(|theme_path| {
                let mut path = self.installed_dir.clone();
                path.extend([Path::new(extension_id.as_ref()), theme_path.as_std_path()]);
                path
            }));
            icon_themes_to_add.extend(extension.manifest.icon_themes.iter().map(
                |icon_theme_path| {
                    let mut path = self.installed_dir.clone();
                    path.extend([
                        Path::new(extension_id.as_ref()),
                        icon_theme_path.as_std_path(),
                    ]);

                    let mut icons_root_path = self.installed_dir.clone();
                    icons_root_path.extend([Path::new(extension_id.as_ref())]);

                    (path, icons_root_path)
                },
            ));
            snippets_to_add.extend(extension.manifest.snippets.iter().flat_map(|snippets| {
                snippets.paths().map(|snippets_path| {
                    let mut path = self.installed_dir.clone();
                    path.extend([Path::new(extension_id.as_ref()), snippets_path.as_path()]);
                    path
                })
            }));
        }

        for (name, entry) in &languages_to_readd {
            let Some(grammar_name) = entry.grammar.clone() else {
                continue;
            };
            if !grammars_to_remove.contains(&grammar_name) {
                continue;
            }
            let owner = std::iter::once(&entry.extension)
                .chain(new_index.extensions.keys())
                .find(|id| {
                    new_index
                        .extensions
                        .get(id.as_ref())
                        .is_some_and(|extension| {
                            extension.manifest.grammars.contains_key(&grammar_name)
                        })
                });
            let Some(owner) = owner else {
                log::warn!(
                    "not re-registering grammar {grammar_name} for language {name}: no installed extension provides it"
                );
                continue;
            };
            let mut grammar_path = self.installed_dir.clone();
            grammar_path.extend([owner.as_ref(), "grammars"]);
            grammar_path.push(grammar_name.as_ref());
            grammar_path.set_extension("wasm");
            grammars_to_add.push((grammar_name, grammar_path));
        }

        self.proxy.register_grammars(grammars_to_add);
        let languages_to_add = new_index
            .languages
            .iter()
            .filter(|(_, entry)| extensions_to_load.contains(&entry.extension))
            .chain(languages_to_readd.iter().map(|(name, entry)| (name, entry)))
            .map(|(name, entry)| {
                let mut language_path = self.installed_dir.clone();
                language_path.extend([Path::new(entry.extension.as_ref()), entry.path.as_path()]);
                (name, entry, language_path)
            })
            .collect::<Vec<_>>();
        let registrations = languages_to_add
            .iter()
            .map(
                |(name, entry, language_path)| language::LanguageRegistration {
                    name: (*name).clone(),
                    grammar_name: entry.grammar.clone(),
                    matcher: entry.matcher.clone(),
                    hidden: entry.hidden,
                    load: Arc::new({
                        let fs = self.fs.clone();
                        let language_path = language_path.clone();
                        let query_files = entry.query_files;
                        move || {
                            let fs = fs.clone();
                            let language_path = language_path.clone();
                            async move {
                                    load_plugin_language(fs, &language_path, query_files).await
                                }
                                .boxed()
                        }
                    }),
                },
            )
            .collect();
        let update = self
            .proxy
            .update_languages(&languages_to_remove, &[], registrations);
        debug_assert_eq!(update.registrations().len(), languages_to_add.len());
        let readded_statuses = update.registrations().iter().skip(
            languages_to_add
                .len()
                .saturating_sub(languages_to_readd.len()),
        );
        let semantic_token_rules_paths = languages_to_add
            .into_iter()
            .zip(update.registrations())
            .filter(|(_, status)| status.is_registered())
            .map(|((name, _, mut rules_path), _)| {
                rules_path.push(SemanticTokenRules::FILE_NAME);
                (name.clone(), rules_path)
            })
            .collect::<Vec<_>>();
        // Rejected re-additions are shadowed by a native language whose rules must survive.
        let mut semantic_token_rules_to_remove = update
            .removed()
            .iter()
            .chain(
                languages_to_readd
                    .iter()
                    .zip(readded_statuses)
                    .filter(|(_, status)| status.is_registered())
                    .map(|((name, _), _)| name),
            )
            .peekable();
        if semantic_token_rules_to_remove.peek().is_some() {
            SettingsStore::update_global(cx, |store, _cx| {
                for language in semantic_token_rules_to_remove {
                    store.remove_language_semantic_token_rules(language.as_ref());
                }
            });
        }
        let fs = self.fs.clone();
        let wasm_host = self.wasm_host.clone();
        let root_dir = self.installed_dir.clone();
        let proxy = self.proxy.clone();
        let extension_entries = extensions_to_load
            .iter()
            .filter_map(|name| new_index.extensions.get(name).cloned())
            .collect::<Vec<_>>();
        self.extension_index = new_index;
        cx.notify();
        cx.emit(Event::ExtensionsUpdated);
        if remote_sync_changed {
            self.sync_remote_clients();
        }

        cx.spawn(async move |this, cx| {
            let semantic_token_rules_to_add = cx
                .background_spawn({
                    let fs = fs.clone();
                    async move {
                        let _ = join_all(server_removal_tasks).await;
                        for theme_path in themes_to_add {
                            proxy
                                .load_user_theme(theme_path, fs.clone())
                                .await
                                .log_err();
                        }

                        for (icon_theme_path, icons_root_path) in icon_themes_to_add {
                            proxy
                                .load_icon_theme(icon_theme_path, icons_root_path, fs.clone())
                                .await
                                .log_err();
                        }

                        for snippets_path in &snippets_to_add {
                            match fs
                                .load(snippets_path)
                                .await
                                .with_context(|| format!("Loading snippets from {snippets_path:?}"))
                            {
                                Ok(snippets_contents) => {
                                    proxy
                                        .register_snippet(snippets_path, &snippets_contents)
                                        .log_err();
                                }
                                Err(e) => log::error!("Cannot load snippets: {e:#}"),
                            }
                        }

                        // Load semantic token rules if present in the language directory.
                        let mut semantic_token_rules_to_add = Vec::new();
                        for (language_name, rules_path) in semantic_token_rules_paths {
                            if !fs.is_file(&rules_path).await {
                                continue;
                            }
                            let rules = fs
                                .load(&rules_path)
                                .await
                                .and_then(|content| SemanticTokenRules::parse(&content));
                            if let Some(rules) = rules.log_err() {
                                semantic_token_rules_to_add.push((language_name, rules));
                            }
                        }
                        semantic_token_rules_to_add
                    }
                })
                .await;

            // Register semantic token rules for newly loaded extension languages.
            if !semantic_token_rules_to_add.is_empty() {
                this.update(cx, |_, cx| {
                    SettingsStore::update_global(cx, |store, _| {
                        for (language_name, rules) in semantic_token_rules_to_add {
                            store.set_language_semantic_token_rules(language_name.0.clone(), rules);
                        }
                    })
                })
                .ok();
            }

            let mut wasm_extensions = Vec::new();
            for extension in extension_entries {
                if extension.manifest.lib.kind.is_none() {
                    continue;
                };

                let extension_path = root_dir.join(extension.manifest.id.as_ref());
                let wasm_extension = WasmExtension::load(
                    &extension_path,
                    &extension.manifest,
                    wasm_host.clone(),
                    cx,
                )
                .await
                .with_context(|| format!("Loading extension from {extension_path:?}"));

                match wasm_extension {
                    Ok(wasm_extension) => {
                        wasm_extensions.push((extension.manifest.clone(), wasm_extension))
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to load extension: {}, {:#}",
                            extension.manifest.id,
                            e
                        );
                        this.update(cx, |_, cx| {
                            cx.emit(Event::ExtensionFailedToLoad(extension.manifest.id.clone()))
                        })
                        .ok();
                    }
                }
            }

            this.update(cx, |this, cx| {
                this.reload_complete_senders.clear();

                for (manifest, wasm_extension) in &wasm_extensions {
                    let extension = Arc::new(wasm_extension.clone());

                    for (language_server_id, language_server_config) in &manifest.language_servers {
                        for language in language_server_config.languages() {
                            this.proxy.register_language_server(
                                extension.clone(),
                                language_server_id.clone(),
                                language.clone(),
                            );
                        }
                    }

                    for id in manifest.context_servers.keys() {
                        this.proxy
                            .register_context_server(extension.clone(), id.clone(), cx);
                    }

                    for (debug_adapter, meta) in &manifest.debug_adapters {
                        let mut path = root_dir.clone();
                        path.push(Path::new(manifest.id.as_ref()));
                        if let Some(schema_path) = &meta.schema_path {
                            path.push(schema_path);
                        } else {
                            path.push("debug_adapter_schemas");
                            path.push(Path::new(debug_adapter.as_ref()).with_extension("json"));
                        }

                        this.proxy.register_debug_adapter(
                            extension.clone(),
                            debug_adapter.clone(),
                            &path,
                        );
                    }

                    for debug_adapter in manifest.debug_locators.keys() {
                        this.proxy
                            .register_debug_locator(extension.clone(), debug_adapter.clone());
                    }
                }

                this.wasm_extensions.extend(wasm_extensions);
                // Observers (extension previews waiting at startup, for one)
                // rely on this to learn that extensions finished loading.
                cx.notify();
                this.proxy.set_extensions_loaded();
                this.proxy.reload_current_theme(cx);
                this.proxy.reload_current_icon_theme(cx);
                trigger_suppressed_extension_removal(this, cx);

                if let Some(events) = ExtensionEvents::try_global(cx) {
                    events.update(cx, |this, cx| {
                        this.emit(extension::Event::ExtensionsInstalledChanged, cx)
                    });
                }
            })
            .ok();
        })
    }

    fn rebuild_extension_index(&self, cx: &mut Context<Self>) -> Task<ExtensionIndex> {
        let fs = self.fs.clone();
        let work_dir = self.wasm_host.work_dir.clone();
        let extensions_dir = self.installed_dir.clone();
        let index_path = self.index_path.clone();
        let proxy = self.proxy.clone();
        cx.background_spawn(async move {
            let start_time = Instant::now();
            let mut index = ExtensionIndex::default();

            fs.create_dir(&work_dir).await.log_err();
            fs.create_dir(&extensions_dir).await.log_err();

            let extension_paths = fs.read_dir(&extensions_dir).await;
            if let Ok(mut extension_paths) = extension_paths {
                while let Some(extension_dir) = extension_paths.next().await {
                    let Ok(extension_dir) = extension_dir else {
                        continue;
                    };

                    if extension_dir
                        .file_name()
                        .is_some_and(|file_name| file_name == ".DS_Store")
                    {
                        continue;
                    }

                    Self::add_extension_to_index(
                        fs.clone(),
                        extension_dir,
                        &mut index,
                        proxy.clone(),
                    )
                    .await
                    .log_err();
                }
            }

            if let Ok(index_json) = serde_json::to_string_pretty(&index) {
                fs.save(&index_path, &index_json.as_str().into(), Default::default())
                    .await
                    .context("failed to save extension index")
                    .log_err();
            }

            log::info!("rebuilt extension index in {:?}", start_time.elapsed());
            index
        })
    }

    async fn add_extension_to_index(
        fs: Arc<dyn Fs>,
        extension_dir: PathBuf,
        index: &mut ExtensionIndex,
        proxy: Arc<ExtensionHostProxy>,
    ) -> Result<()> {
        let mut extension_manifest = ExtensionManifest::load(fs.clone(), &extension_dir).await?;
        let extension_id = extension_manifest.id.clone();

        // TODO: distinguish dev extensions more explicitly, by the absence
        // of a checksum file that we'll create when downloading normal extensions.
        let is_dev = fs
            .metadata(&extension_dir)
            .await?
            .with_context(|| format!("missing extension directory {extension_dir:?}"))?
            .is_symlink;

        let language_dir = extension_dir.join("languages");
        if let Ok(mut language_paths) = fs.read_dir(&language_dir).await {
            while let Some(language_path) = language_paths.next().await {
                let language_path = language_path
                    .with_context(|| format!("reading entries in language dir {language_dir:?}"))?;
                let Ok(relative_path) = language_path.strip_prefix(&extension_dir) else {
                    continue;
                };
                let Ok(Some(fs_metadata)) = fs.metadata(&language_path).await else {
                    continue;
                };
                if !fs_metadata.is_dir {
                    continue;
                }
                let config = {
                    let fs = fs.clone();
                    let language_config_path = language_path.join(LanguageConfig::FILE_NAME);
                    async move {
                        let config = fs.load(&language_config_path).await.with_context(|| {
                            format!("loading language config from {language_config_path:?}")
                        })?;
                        LanguageConfig::from_toml(&config).map_err(anyhow::Error::from)
                    }
                };
                let query_files = async {
                    Ok(discover_query_files(fs.clone(), &language_path)
                        .await
                        .log_err())
                };
                let (config, query_files) = futures::try_join!(config, query_files)?;

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.languages.contains(&relative_path) {
                    extension_manifest.languages.push(relative_path.clone());
                }

                index.languages.insert(
                    config.name.clone(),
                    ExtensionIndexLanguageEntry {
                        extension: extension_id.clone(),
                        path: relative_path.as_std_path().to_path_buf(),
                        matcher: config.matcher,
                        hidden: config.hidden,
                        grammar: config.grammar,
                        query_files,
                    },
                );
            }
        }

        if let Ok(mut theme_paths) = fs.read_dir(&extension_dir.join("themes")).await {
            while let Some(theme_path) = theme_paths.next().await {
                let theme_path = theme_path?;
                let Ok(relative_path) = theme_path.strip_prefix(&extension_dir) else {
                    continue;
                };

                let Some(theme_families) = proxy
                    .list_theme_names(theme_path.clone(), fs.clone())
                    .await
                    .log_err()
                else {
                    continue;
                };

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.themes.contains(&relative_path) {
                    extension_manifest.themes.push(relative_path.clone());
                }

                for theme_name in theme_families {
                    index.themes.insert(
                        theme_name.into(),
                        ExtensionIndexThemeEntry {
                            extension: extension_id.clone(),
                            path: relative_path.as_std_path().to_path_buf(),
                        },
                    );
                }
            }
        }

        if let Ok(mut icon_theme_paths) = fs.read_dir(&extension_dir.join("icon_themes")).await {
            while let Some(icon_theme_path) = icon_theme_paths.next().await {
                let icon_theme_path = icon_theme_path?;
                let Ok(relative_path) = icon_theme_path.strip_prefix(&extension_dir) else {
                    continue;
                };

                let Some(icon_theme_families) = proxy
                    .list_icon_theme_names(icon_theme_path.clone(), fs.clone())
                    .await
                    .log_err()
                else {
                    continue;
                };

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.icon_themes.contains(&relative_path) {
                    extension_manifest.icon_themes.push(relative_path.clone());
                }

                for icon_theme_name in icon_theme_families {
                    index.icon_themes.insert(
                        icon_theme_name.into(),
                        ExtensionIndexIconThemeEntry {
                            extension: extension_id.clone(),
                            path: relative_path.as_std_path().to_path_buf(),
                        },
                    );
                }
            }
        }

        let extension_wasm_path = extension_dir.join("extension.wasm");
        if fs.is_file(&extension_wasm_path).await {
            extension_manifest
                .lib
                .kind
                .get_or_insert(ExtensionLibraryKind::Rust);
        }

        index.extensions.insert(
            extension_id.clone(),
            ExtensionIndexEntry {
                dev: is_dev,
                manifest: Arc::new(extension_manifest),
                disabled: false,
            },
        );

        Ok(())
    }

    fn prepare_remote_extension(
        &mut self,
        extension_id: Arc<str>,
        is_dev: bool,
        tmp_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let src_dir = self.extensions_dir().join(extension_id.as_ref());
        let Some(loaded_extension) = self.extension_index.extensions.get(&extension_id).cloned()
        else {
            return Task::ready(Err(anyhow!("extension no longer installed")));
        };
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            const EXTENSION_TOML: &str = "extension.toml";
            const EXTENSION_WASM: &str = "extension.wasm";
            const CONFIG_TOML: &str = LanguageConfig::FILE_NAME;

            if is_dev {
                let manifest_toml = toml::to_string(&loaded_extension.manifest)?;
                fs.save(
                    &tmp_dir.join(EXTENSION_TOML),
                    &Rope::from(manifest_toml),
                    language::LineEnding::Unix,
                )
                .await?;
            } else {
                fs.copy_file(
                    &src_dir.join(EXTENSION_TOML),
                    &tmp_dir.join(EXTENSION_TOML),
                    fs::CopyOptions::default(),
                )
                .await?
            }

            if fs.is_file(&src_dir.join(EXTENSION_WASM)).await {
                fs.copy_file(
                    &src_dir.join(EXTENSION_WASM),
                    &tmp_dir.join(EXTENSION_WASM),
                    fs::CopyOptions::default(),
                )
                .await?
            }

            for language_path in loaded_extension.manifest.languages.iter() {
                if fs
                    .is_file(&src_dir.join(language_path).join(CONFIG_TOML))
                    .await
                {
                    fs.create_dir(&tmp_dir.join(language_path)).await?;
                    fs.copy_file(
                        &src_dir.join(language_path).join(CONFIG_TOML),
                        &tmp_dir.join(language_path).join(CONFIG_TOML),
                        fs::CopyOptions::default(),
                    )
                    .await?
                }
            }

            for (adapter_name, meta) in loaded_extension.manifest.debug_adapters.iter() {
                let schema_path = extension::build_debug_adapter_schema_path(adapter_name, meta)?;

                if fs.is_file(&src_dir.join(&schema_path)).await {
                    if let Some(parent) = schema_path.parent() {
                        fs.create_dir(&tmp_dir.join(parent)).await?
                    }
                    fs.copy_file(
                        &src_dir.join(&schema_path),
                        &tmp_dir.join(&schema_path),
                        fs::CopyOptions::default(),
                    )
                    .await?
                }
            }

            Ok(())
        })
    }

    fn sync_remote_clients(&mut self) {
        for state in self.remote_clients.values() {
            state
                .dirty_tx
                .unbounded_send(RemoteSyncSignal::IndexChanged)
                .ok();
        }
    }

    async fn reconcile_remote_client(
        this: WeakEntity<Self>,
        client: WeakEntity<RemoteClient>,
        mut dirty_rx: UnboundedReceiver<RemoteSyncSignal>,
        cx: &mut AsyncApp,
    ) {
        let mut failed_attempts = 0_usize;
        loop {
            while let Ok(signal) = dirty_rx.try_recv() {
                if signal == RemoteSyncSignal::Reconnected {
                    failed_attempts = 0;
                }
            }

            let Ok(connection_state) =
                client.read_with(cx, |client, _cx| client.connection_state())
            else {
                return;
            };
            if connection_state == ConnectionState::Disconnected
                || connection_state == ConnectionState::Reconnecting
            {
                failed_attempts = 0;
                if dirty_rx.next().await.is_none() {
                    return;
                }
                continue;
            }

            match Self::sync_extensions_to_remote(&this, client.clone(), cx).await {
                Ok(()) => {
                    failed_attempts = 0;
                    if dirty_rx.next().await.is_none() {
                        return;
                    }
                }
                Err(error) => {
                    failed_attempts += 1;
                    if failed_attempts >= MAX_REMOTE_SYNC_ATTEMPTS {
                        log::error!(
                            "Failed to sync extensions to a remote client {failed_attempts} times, waiting for an extension or connection change before retrying: {error:#}"
                        );
                        match dirty_rx.next().await {
                            None => return,
                            Some(RemoteSyncSignal::Reconnected) => failed_attempts = 0,
                            Some(RemoteSyncSignal::IndexChanged) => {}
                        }
                        continue;
                    }
                    let delay = remote_sync_retry_delay(failed_attempts - 1);
                    log::error!(
                        "Failed to sync extensions to a remote client (attempt {failed_attempts}), will retry in {delay:?}: {error:#}"
                    );
                    let timer = cx.background_executor().timer(delay).fuse();
                    futures::pin_mut!(timer);
                    loop {
                        select_biased! {
                            signal = dirty_rx.next() => {
                                match signal {
                                    None => return,
                                    Some(RemoteSyncSignal::Reconnected) => {
                                        failed_attempts = 0;
                                        break;
                                    }
                                    Some(RemoteSyncSignal::IndexChanged) => {}
                                }
                            }
                            _ = timer => break,
                        }
                    }
                }
            }
        }
    }

    async fn sync_extensions_to_remote(
        this: &WeakEntity<Self>,
        client: WeakEntity<RemoteClient>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let entries = this.update(cx, |this, _cx| {
            this.extension_index
                .extensions_to_sync_to_remote()
                .into_entries()
                .collect::<Vec<_>>()
        })?;
        let mut prepared_dev_payloads = HashMap::default();
        let mut extensions = Vec::new();
        for (id, entry) in entries {
            let mut content_fingerprint = None;
            if entry.dev {
                match Self::prepare_dev_extension_payload(this, &id, cx).await {
                    Ok((payload_dir, fingerprint)) => {
                        content_fingerprint = Some(fingerprint);
                        prepared_dev_payloads.insert(id.to_string(), payload_dir);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to prepare dev extension {id} for a remote sync: {error:#}"
                        );
                    }
                }
            }
            extensions.push(proto::Extension {
                id: id.to_string(),
                version: entry.manifest.version.to_string(),
                dev: entry.dev,
                content_fingerprint,
            });
        }

        let request = client.update(cx, |client, _cx| {
            client
                .proto_client()
                .request(proto::SyncExtensions { extensions })
        })?;
        let response = with_remote_sync_timeout(
            cx,
            REMOTE_SYNC_TIMEOUT,
            "requesting the remote extension list",
            request,
        )
        .await?;
        let path_style = client.read_with(cx, |client, _| client.path_style())?;

        let mut failed_installs = Vec::new();
        for missing_extension in response.missing_extensions.into_iter() {
            let prepared_payload = prepared_dev_payloads.remove(&missing_extension.id);
            if let Err(error) = Self::install_extension_on_remote(
                this,
                &client,
                &missing_extension,
                &response.tmp_dir,
                path_style,
                prepared_payload,
                cx,
            )
            .await
            {
                log::error!(
                    "Failed to install extension {} on the remote: {error:#}",
                    missing_extension.id
                );
                failed_installs.push(missing_extension.id);
            }
        }
        if !prepared_dev_payloads.is_empty() {
            cx.background_executor()
                .spawn(async move { drop(prepared_dev_payloads) })
                .detach();
        }

        anyhow::ensure!(
            failed_installs.is_empty(),
            "failed to install extensions on the remote: {failed_installs:?}"
        );
        anyhow::Ok(())
    }

    async fn prepare_dev_extension_payload(
        this: &WeakEntity<Self>,
        id: &Arc<str>,
        cx: &mut AsyncApp,
    ) -> Result<(tempfile::TempDir, u64)> {
        let payload_dir = cx
            .background_executor()
            .spawn(async move { tempfile::tempdir() })
            .await?;
        this.update(cx, |this, cx| {
            this.prepare_remote_extension(id.clone(), true, payload_dir.path().to_owned(), cx)
        })?
        .await?;
        let fs = this.read_with(cx, |this, _cx| this.fs.clone())?;
        let fingerprint = cx
            .background_executor()
            .spawn({
                let path = payload_dir.path().to_owned();
                async move { hash_directory_contents(&fs, &path).await }
            })
            .await?;
        Ok((payload_dir, fingerprint))
    }

    async fn install_extension_on_remote(
        this: &WeakEntity<Self>,
        client: &WeakEntity<RemoteClient>,
        missing_extension: &proto::Extension,
        remote_tmp_dir: &str,
        path_style: PathStyle,
        prepared_payload: Option<tempfile::TempDir>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let already_prepared = prepared_payload.is_some();
        let tmp_dir = match prepared_payload {
            Some(payload_dir) => payload_dir,
            None => {
                cx.background_executor()
                    .spawn(async move { tempfile::tempdir() })
                    .await?
            }
        };
        let result = Self::upload_extension_to_remote(
            this,
            client,
            missing_extension,
            remote_tmp_dir,
            path_style,
            tmp_dir.path().to_owned(),
            already_prepared,
            cx,
        )
        .await;
        cx.background_executor()
            .spawn(async move { drop(tmp_dir) })
            .detach();
        result
    }

    async fn upload_extension_to_remote(
        this: &WeakEntity<Self>,
        client: &WeakEntity<RemoteClient>,
        missing_extension: &proto::Extension,
        remote_tmp_dir: &str,
        path_style: PathStyle,
        local_dir: PathBuf,
        already_prepared: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        static UPLOAD_NONCE: AtomicU64 = AtomicU64::new(0);

        anyhow::ensure!(
            !path_style.has_root(Path::new(&missing_extension.id)),
            "extension id must not be rooted: {:?}",
            missing_extension.id
        );

        if !already_prepared {
            this.update(cx, |this, cx| {
                this.prepare_remote_extension(
                    missing_extension.id.clone().into(),
                    missing_extension.dev,
                    local_dir.clone(),
                    cx,
                )
            })?
            .await?;
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let upload_name = format!(
            "{}-{}-{}-{}",
            missing_extension.id,
            std::process::id(),
            UPLOAD_NONCE.fetch_add(1, AtomicOrdering::Relaxed),
            timestamp,
        );
        let dest_dir = RemotePathBuf::new(
            path_style
                .join(remote_tmp_dir, &upload_name)
                .to_string_lossy()
                .into_owned(),
            path_style,
        );
        log::info!(
            "Uploading extension {} to {:?}",
            missing_extension.id,
            dest_dir
        );

        let upload = client.update(cx, |client, cx| {
            client.upload_directory(local_dir, dest_dir.clone(), cx)
        })?;
        with_remote_sync_timeout(cx, REMOTE_SYNC_TIMEOUT, "uploading an extension", upload).await?;

        log::info!("Finished uploading extension {}", missing_extension.id);

        let install = client.update(cx, |client, _cx| {
            client.proto_client().request(proto::InstallExtension {
                tmp_dir: dest_dir.to_proto(),
                extension: Some(missing_extension.clone()),
            })
        })?;
        with_remote_sync_timeout(cx, REMOTE_SYNC_TIMEOUT, "installing an extension", install)
            .await?;
        Ok(())
    }

    pub fn register_remote_client(&mut self, client: Entity<RemoteClient>, cx: &mut Context<Self>) {
        let entity_id = client.entity_id();
        if self.remote_clients.contains_key(&entity_id) {
            return;
        }

        let (dirty_tx, dirty_rx) = unbounded();

        let event_subscription = cx.subscribe(&client, |store, client, event, _cx| match event {
            RemoteClientEvent::Reconnected => {
                if let Some(state) = store.remote_clients.get(&client.entity_id()) {
                    state
                        .dirty_tx
                        .unbounded_send(RemoteSyncSignal::Reconnected)
                        .ok();
                }
            }
            RemoteClientEvent::Disconnected { .. } => {}
        });
        let release_subscription = cx.observe_release(&client, move |store, _client, _cx| {
            store.remote_clients.remove(&entity_id);
        });

        let task = cx.spawn({
            let client = client.downgrade();
            let initial_index_load = self.initial_index_load.clone();
            async move |this, cx| {
                initial_index_load.await;
                Self::reconcile_remote_client(this, client, dirty_rx, cx).await;
            }
        });

        self.remote_clients.insert(
            entity_id,
            RemoteClientState {
                dirty_tx,
                _task: task,
                _subscriptions: Subscription::join(event_subscription, release_subscription),
            },
        );
    }
}

async fn load_plugin_language(
    fs: Arc<dyn Fs>,
    language_path: &Path,
    query_files: Option<QueryFiles>,
) -> Result<LoadedLanguage> {
    let config = {
        let fs = fs.clone();
        let config_path = language_path.join(LanguageConfig::FILE_NAME);
        async move {
            let contents = fs.load(&config_path).await?;
            LanguageConfig::from_toml(&contents).map_err(anyhow::Error::from)
        }
    };
    let context_provider = {
        let fs = fs.clone();
        let tasks_path = language_path.join(TaskTemplates::FILE_NAME);
        async move {
            fs.load(&tasks_path).await.ok().and_then(|contents| {
                serde_json_lenient::from_str(&contents)
                    .log_err()
                    .map(|definitions| {
                        Arc::new(ContextProviderWithTasks::new(definitions)) as Arc<_>
                    })
            })
        }
    };
    let (config, queries, context_provider) = futures::try_join!(
        config,
        async move { Ok(load_plugin_queries(fs, &language_path, query_files).await) },
        async move { Ok(context_provider.await) }
    )?;

    Ok(LoadedLanguage {
        config,
        queries,
        context_provider,
        toolchain_provider: None,
        manifest_name: None,
    })
}

async fn discover_query_files(fs: Arc<dyn Fs>, root_path: &Path) -> Result<QueryFiles> {
    let mut paths = fs.read_dir(root_path).await?;
    let mut query_files = QueryFiles::empty();
    while let Some(path) = paths.next().await {
        let path = path?;
        let Some(query_file) = path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .and_then(|file_name| file_name.parse::<QueryFile>().ok())
        else {
            continue;
        };
        query_files.insert(query_file.into());
    }
    Ok(query_files)
}

async fn load_plugin_queries(
    fs: Arc<dyn Fs>,
    root_path: &Path,
    query_files: Option<QueryFiles>,
) -> LanguageQueries {
    let query_files = query_files.unwrap_or_else(QueryFiles::all);
    let files = join_all(query_files.query_files().map(|query_file| {
        let fs = fs.clone();
        let path = root_path.join(query_file.file_name());
        async move {
            fs.load(&path)
                .await
                .ok()
                .map(|contents| QueryFileContents::new(query_file, Cow::Owned(contents)))
        }
    }))
    .await;
    LanguageQueries::from_files(files.into_iter().flatten())
}
