use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use cloud_api_types::{ExtensionApiManifest, ExtensionMetadata, ExtensionProvides};
use collections::{BTreeMap, BTreeSet, HashMap};
use fs::Fs;
use futures::{AsyncReadExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use util::ResultExt as _;

/// The only static index schema this client understands.
pub const STATIC_INDEX_SCHEMA: u32 = 1;

/// Host requirements this build can satisfy. An extension lists the hosts it
/// needs in `requires` in its `extension.toml`; `"lathe"` marks an extension
/// that only works in Lathe. Anything not listed here comes from a newer
/// extension format and must not be installed.
pub const SUPPORTED_REQUIREMENTS: &[&str] = &["lathe"];

pub const LATHE_REQUIREMENT: &str = "lathe";

pub fn unsupported_requirements(requires: &[Arc<str>]) -> Vec<Arc<str>> {
    requires
        .iter()
        .filter(|requirement| !SUPPORTED_REQUIREMENTS.contains(&requirement.as_ref()))
        .cloned()
        .collect()
}

/// Lathe-specific `extension.toml` fields. They are read separately from
/// [`extension::ExtensionManifest`] so that type stays identical to upstream Zed.
#[derive(Debug, Default, Deserialize)]
pub struct LatheManifestFields {
    #[serde(default)]
    pub requires: Vec<Arc<str>>,
    /// Permissions for the Lathe extension API, e.g. `"buffer:write"`.
    #[serde(default)]
    pub lathe_capabilities: Vec<Arc<str>>,
}

impl LatheManifestFields {
    /// Reads the Lathe fields from `extension.toml` in `extension_dir`. Legacy
    /// `extension.json` manifests predate these fields and yield the defaults.
    pub async fn load(fs: &dyn Fs, extension_dir: &Path) -> Result<Self> {
        let manifest_path = extension_dir.join("extension.toml");
        if !fs.is_file(&manifest_path).await {
            return Ok(Self::default());
        }
        let content = fs
            .load(&manifest_path)
            .await
            .with_context(|| format!("loading {manifest_path:?}"))?;
        toml::from_str(&content).with_context(|| format!("parsing {manifest_path:?}"))
    }
}

/// A registry published as a single JSON file, e.g. on GitHub Pages.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StaticIndex {
    pub schema: u32,
    #[serde(default)]
    pub extensions: Vec<StaticIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct StaticIndexEntry {
    pub id: Arc<str>,
    pub name: String,
    pub version: Arc<str>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    pub repository: String,
    #[serde(default)]
    pub schema_version: Option<i32>,
    #[serde(default)]
    pub wasm_api_version: Option<String>,
    #[serde(default)]
    pub lathe_api_version: Option<String>,
    #[serde(default)]
    pub requires: Vec<Arc<str>>,
    #[serde(default)]
    pub lathe_capabilities: Vec<Arc<str>>,
    #[serde(default)]
    pub provides: BTreeSet<ExtensionProvides>,
    pub download_url: String,
    pub sha256: Arc<str>,
    pub published_at: DateTime<Utc>,
}

impl StaticIndexEntry {
    pub fn is_lathe_only(&self) -> bool {
        self.requires
            .iter()
            .any(|requirement| requirement.as_ref() == LATHE_REQUIREMENT)
    }

    pub fn to_metadata(&self) -> ExtensionMetadata {
        ExtensionMetadata {
            id: self.id.clone(),
            manifest: ExtensionApiManifest {
                name: self.name.clone(),
                version: self.version.clone(),
                description: self.description.clone(),
                authors: self.authors.clone(),
                repository: self.repository.clone(),
                schema_version: self.schema_version,
                wasm_api_version: self.wasm_api_version.clone(),
                provides: self.provides.clone(),
            },
            published_at: self.published_at,
            download_count: 0,
        }
    }

    pub fn matches(
        &self,
        search: Option<&str>,
        provides_filter: Option<&BTreeSet<ExtensionProvides>>,
    ) -> bool {
        if let Some(provides_filter) = provides_filter
            && !provides_filter.is_empty()
            && self.provides.is_disjoint(provides_filter)
        {
            return false;
        }

        let Some(search) = search.map(str::trim).filter(|search| !search.is_empty()) else {
            return true;
        };

        if let Some(id) = search.strip_prefix("id:") {
            return self.id.as_ref() == id.trim();
        }

        let search = search.to_lowercase();
        self.id.to_lowercase().contains(&search)
            || self.name.to_lowercase().contains(&search)
            || self
                .description
                .as_ref()
                .is_some_and(|description| description.to_lowercase().contains(&search))
    }
}

/// An extension version offered by a static registry, along with the index it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    pub index_url: Arc<str>,
    pub entry: StaticIndexEntry,
}

/// Flattens the given indexes into one entry per extension id. When several
/// indexes offer the same id, the index listed later wins, so a registry can
/// override an extension published by an earlier one.
pub fn collect_registry_entries(
    indexes: impl IntoIterator<Item = (Arc<str>, StaticIndex)>,
) -> HashMap<Arc<str>, RegistryEntry> {
    let mut entries = HashMap::default();
    for (index_url, index) in indexes {
        for entry in index.extensions {
            let unsupported = unsupported_requirements(&entry.requires);
            if !unsupported.is_empty() {
                log::info!(
                    "skipping extension {} from {index_url}: unsupported requirements {unsupported:?}",
                    entry.id
                );
                continue;
            }
            if let Err(error) =
                crate::wasm_host::lathe::LatheCapability::parse_all(&entry.lathe_capabilities)
            {
                log::info!("skipping extension {} from {index_url}: {error}", entry.id);
                continue;
            }
            entries.insert(
                entry.id.clone(),
                RegistryEntry {
                    index_url: index_url.clone(),
                    entry,
                },
            );
        }
    }
    entries
}

/// Merges extensions from the Zed API with those from static registries.
/// Registry entries replace Zed entries that share an id.
pub fn merge_extension_lists(
    zed_extensions: Vec<ExtensionMetadata>,
    registry_extensions: Vec<ExtensionMetadata>,
) -> Vec<ExtensionMetadata> {
    let mut by_id = BTreeMap::default();
    for extension in zed_extensions.into_iter().chain(registry_extensions) {
        by_id.insert(extension.id.clone(), extension);
    }
    by_id.into_values().collect()
}

/// A static index as last received from its registry, stored so that unchanged
/// indexes cost a single `304 Not Modified` and so listings still work offline.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CachedIndex {
    url: String,
    #[serde(default)]
    etag: Option<String>,
    index: StaticIndex,
}

fn cache_path(cache_dir: &Path, url: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(url.as_bytes()));
    cache_dir.join(format!("{key}.json"))
}

async fn load_cached_index(fs: &dyn Fs, path: &Path, url: &str) -> Option<CachedIndex> {
    let content = fs.load(path).await.ok()?;
    let cached: CachedIndex = serde_json::from_str(&content)
        .with_context(|| format!("parsing cached extension registry {path:?}"))
        .log_err()?;
    (cached.url == url && cached.index.schema == STATIC_INDEX_SCHEMA).then_some(cached)
}

/// Fetches a static index, revalidating against the on-disk copy in `cache_dir`
/// with `If-None-Match`. If the registry cannot be reached, the cached copy is
/// returned instead so installed registry extensions keep their listing.
pub async fn fetch_static_index(
    http_client: &dyn HttpClient,
    fs: &dyn Fs,
    cache_dir: &Path,
    url: &str,
) -> Result<StaticIndex> {
    let path = cache_path(cache_dir, url);
    let cached = load_cached_index(fs, &path, url).await;

    match fetch_static_index_from_network(
        http_client,
        url,
        cached.as_ref().and_then(|cached| cached.etag.as_deref()),
    )
    .await
    {
        Ok(None) => {
            let cached = cached.with_context(|| {
                format!("extension registry {url} returned 304 without a cached copy")
            })?;
            Ok(cached.index)
        }
        Ok(Some((index, etag))) => {
            let cached = CachedIndex {
                url: url.to_string(),
                etag,
                index,
            };
            match serde_json::to_string(&cached) {
                Ok(content) => {
                    fs.create_dir(cache_dir).await.log_err();
                    fs.save(&path, &content.as_str().into(), Default::default())
                        .await
                        .with_context(|| format!("caching extension registry {url}"))
                        .log_err();
                }
                Err(error) => log::error!("serializing extension registry {url}: {error}"),
            }
            Ok(cached.index)
        }
        Err(error) => match cached {
            Some(cached) => {
                log::warn!("{error:#}; using cached copy");
                Ok(cached.index)
            }
            None => Err(error),
        },
    }
}

/// Returns `None` when the registry reports the index is unchanged.
async fn fetch_static_index_from_network(
    http_client: &dyn HttpClient,
    url: &str,
    etag: Option<&str>,
) -> Result<Option<(StaticIndex, Option<String>)>> {
    let mut request = http_client::Builder::new()
        .uri(url)
        .follow_redirects(http_client::RedirectPolicy::FollowAll);
    if let Some(etag) = etag {
        request = request.header(http_client::http::header::IF_NONE_MATCH, etag);
    }
    let request = request.body(AsyncBody::empty())?;

    let mut response = http_client
        .send(request)
        .await
        .with_context(|| format!("fetching extension registry {url}"))?;

    if response.status() == http_client::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .with_context(|| format!("reading extension registry {url}"))?;

    if !response.status().is_success() {
        bail!(
            "extension registry {url} returned status {}",
            response.status().as_u16()
        );
    }

    let etag = response
        .headers()
        .get(http_client::http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let index: StaticIndex = serde_json::from_slice(&body)
        .with_context(|| format!("parsing extension registry {url}"))?;
    if index.schema != STATIC_INDEX_SCHEMA {
        bail!(
            "extension registry {url} uses unsupported schema {}, expected {STATIC_INDEX_SCHEMA}",
            index.schema
        );
    }
    Ok(Some((index, etag)))
}

/// Downloads an extension archive and, when a checksum is known, verifies it.
pub async fn download_archive(
    http_client: &dyn HttpClient,
    url: &str,
    expected_sha256: Option<&str>,
) -> Result<Vec<u8>> {
    let mut response = http_client
        .get(url, AsyncBody::default(), true)
        .await
        .context("downloading extension")?;

    if !response.status().is_success() {
        bail!(
            "downloading extension from {url} returned status {}",
            response.status().as_u16()
        );
    }

    let content_length = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<usize>().ok());

    let mut body = BufReader::new(response.body_mut());
    let mut tar_gz_bytes = Vec::new();
    body.read_to_end(&mut tar_gz_bytes).await?;

    if let Some(content_length) = content_length {
        let actual_len = tar_gz_bytes.len();
        if content_length != actual_len {
            bail!(
                "downloaded extension size {actual_len} does not match content length {content_length}"
            );
        }
    }

    if let Some(expected_sha256) = expected_sha256 {
        verify_sha256(&tar_gz_bytes, expected_sha256)?;
    }

    Ok(tar_gz_bytes)
}

pub fn verify_sha256(bytes: &[u8], expected_sha256: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        bail!("extension checksum mismatch: expected {expected_sha256}, got {actual}");
    }
    Ok(())
}

/// Where an installed extension was downloaded from. Extensions without a
/// recorded origin were installed from the Zed API.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallOrigin {
    ZedApi,
    Registry {
        index_url: Arc<str>,
    },
    ArchiveUrl {
        url: Arc<str>,
    },
    /// Cloned from a git repository and built locally as a dev extension.
    GitUrl {
        url: Arc<str>,
        checkout: PathBuf,
    },
}

/// Accepts `https://`, `ssh://` and scp-style `user@host:path` git URLs.
/// Local paths, `file://` and other transports are rejected so this entry
/// point cannot be used to reach the local filesystem.
pub fn validate_git_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.starts_with('-') || url.chars().any(char::is_whitespace) {
        bail!("invalid git URL {url:?}");
    }
    if let Ok(parsed) = url::Url::parse(url) {
        if matches!(parsed.scheme(), "https" | "ssh") && parsed.host_str().is_some() {
            return Ok(());
        }
        bail!("git URL must use https or ssh, got {url:?}");
    }
    let is_scp_style = url.split_once(':').is_some_and(|(user_and_host, path)| {
        user_and_host
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && !host.is_empty() && !host.contains('/'))
            && !path.is_empty()
    });
    if is_scp_style {
        return Ok(());
    }
    bail!("not a git URL: {url:?}")
}

/// A stable, filesystem-safe directory name for a clone of `url`.
pub fn git_checkout_dir_name(url: &str) -> String {
    let repository_name = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git");
    let repository_name = repository_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let digest = format!("{:x}", Sha256::digest(url.trim().as_bytes()));
    let short_digest = digest.get(..12).unwrap_or(&digest);
    if repository_name.is_empty() {
        short_digest.to_string()
    } else {
        format!("{repository_name}-{short_digest}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct InstallRecord {
    #[serde(flatten)]
    pub origin: InstallOrigin,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Arc<str>>,
}

/// Persisted next to the extension index rather than inside each extension's
/// directory, because extension directories are replaced wholesale on upgrade
/// and are synced to remote hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct InstallOrigins {
    #[serde(default)]
    pub extensions: BTreeMap<Arc<str>, InstallRecord>,
}

impl InstallOrigins {
    pub async fn load(fs: &dyn Fs, path: &Path) -> Self {
        let Ok(content) = fs.load(path).await else {
            return Self::default();
        };
        serde_json::from_str(&content)
            .with_context(|| format!("parsing extension origins {path:?}"))
            .log_err()
            .unwrap_or_default()
    }

    pub async fn save(&self, fs: &dyn Fs, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        fs.save(path, &content.as_str().into(), Default::default())
            .await
            .with_context(|| format!("saving extension origins {path:?}"))
    }

    pub fn get(&self, extension_id: &str) -> InstallOrigin {
        self.extensions
            .get(extension_id)
            .map(|record| record.origin.clone())
            .unwrap_or(InstallOrigin::ZedApi)
    }

    pub fn requires(&self, extension_id: &str) -> &[Arc<str>] {
        self.extensions
            .get(extension_id)
            .map(|record| record.requires.as_slice())
            .unwrap_or_default()
    }

    pub fn set(&mut self, extension_id: Arc<str>, origin: InstallOrigin, requires: Vec<Arc<str>>) {
        if origin == InstallOrigin::ZedApi && requires.is_empty() {
            self.extensions.remove(&extension_id);
        } else {
            self.extensions
                .insert(extension_id, InstallRecord { origin, requires });
        }
    }

    pub fn remove(&mut self, extension_id: &str) {
        self.extensions.remove(extension_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, version: &str, provides: &[ExtensionProvides]) -> StaticIndexEntry {
        StaticIndexEntry {
            id: id.into(),
            name: format!("{id} name"),
            version: version.into(),
            description: Some(format!("does {id} things")),
            authors: vec![],
            repository: format!("https://example.com/{id}"),
            schema_version: Some(1),
            wasm_api_version: None,
            lathe_api_version: None,
            requires: vec![],
            lathe_capabilities: vec![],
            provides: provides.iter().copied().collect(),
            download_url: format!("https://example.com/{id}-{version}.tar.gz"),
            sha256: "00".into(),
            published_at: DateTime::<Utc>::default(),
        }
    }

    #[test]
    fn test_later_index_wins_on_collision() {
        let entries = collect_registry_entries([
            (
                Arc::from("https://a.example/index.json"),
                StaticIndex {
                    schema: 1,
                    extensions: vec![entry("shared", "1.0.0", &[]), entry("only-a", "1.0.0", &[])],
                },
            ),
            (
                Arc::from("https://b.example/index.json"),
                StaticIndex {
                    schema: 1,
                    extensions: vec![entry("shared", "2.0.0", &[])],
                },
            ),
        ]);

        let shared = &entries["shared"];
        assert_eq!(shared.index_url.as_ref(), "https://b.example/index.json");
        assert_eq!(shared.entry.version.as_ref(), "2.0.0");
        assert_eq!(
            entries["only-a"].index_url.as_ref(),
            "https://a.example/index.json"
        );
    }

    #[test]
    fn test_registry_replaces_zed_extension() {
        let zed = vec![
            entry("shared", "1.0.0", &[]).to_metadata(),
            entry("zed-only", "1.0.0", &[]).to_metadata(),
        ];
        let registry = vec![entry("shared", "9.0.0", &[]).to_metadata()];

        let merged = merge_extension_lists(zed, registry);
        let versions = merged
            .iter()
            .map(|extension| (extension.id.as_ref(), extension.manifest.version.as_ref()))
            .collect::<Vec<_>>();
        assert_eq!(versions, vec![("shared", "9.0.0"), ("zed-only", "1.0.0")]);
    }

    #[test]
    fn test_entry_matching() {
        let themes = entry("nice-theme", "1.0.0", &[ExtensionProvides::Themes]);

        assert!(themes.matches(None, None));
        assert!(themes.matches(Some("NICE"), None));
        assert!(themes.matches(Some("things"), None));
        assert!(!themes.matches(Some("rust"), None));
        assert!(themes.matches(Some("id:nice-theme"), None));
        assert!(!themes.matches(Some("id:nice"), None));

        let themes_filter = BTreeSet::from_iter([ExtensionProvides::Themes]);
        let languages_filter = BTreeSet::from_iter([ExtensionProvides::Languages]);
        assert!(themes.matches(None, Some(&themes_filter)));
        assert!(!themes.matches(None, Some(&languages_filter)));
    }

    #[test]
    fn test_verify_sha256() {
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(b"hello", expected).is_ok());
        assert!(verify_sha256(b"hello", &expected.to_uppercase()).is_ok());
        assert!(verify_sha256(b"hellO", expected).is_err());
    }

    #[test]
    fn test_static_index_parses_minimal_entry_with_publisher_fields() {
        let index: StaticIndex = serde_json::from_str(
            r#"{
                "schema": 1,
                "generated_at": "2026-11-05T00:00:00Z",
                "extensions": [{
                    "source_commit": "26322c69362fae37951b921379a16c557a2216dc",
                    "id": "insert-date",
                    "name": "Insert Date",
                    "version": "0.1.0",
                    "repository": "https://github.com/example/insert-date",
                    "download_url": "https://example.com/insert-date-0.1.0.tar.gz",
                    "sha256": "abc",
                    "published_at": "2026-11-05T00:00:00Z"
                }]
            }"#,
        )
        .expect("minimal index should parse");
        assert_eq!(index.extensions.len(), 1);
        assert!(index.extensions[0].provides.is_empty());
    }

    #[gpui::test]
    async fn test_static_index_cache(cx: &mut gpui::TestAppContext) {
        use http_client::{FakeHttpClient, Response};
        use parking_lot::Mutex;

        const URL: &str = "https://registry.example/index.json";

        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Server {
            Serve,
            Offline,
        }

        let server = Arc::new(Mutex::new(Server::Serve));
        let seen_if_none_match = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let http_client = FakeHttpClient::create({
            let server = server.clone();
            let seen_if_none_match = seen_if_none_match.clone();
            move |request| {
                let server = *server.lock();
                let if_none_match = request
                    .headers()
                    .get(http_client::http::header::IF_NONE_MATCH)
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                seen_if_none_match.lock().push(if_none_match.clone());
                async move {
                    if server == Server::Offline {
                        anyhow::bail!("connection refused");
                    }
                    if if_none_match.as_deref() == Some("\"v1\"") {
                        return Ok(Response::builder().status(304).body(AsyncBody::empty())?);
                    }
                    let body = serde_json::to_string(&StaticIndex {
                        schema: STATIC_INDEX_SCHEMA,
                        extensions: vec![entry("cached", "1.0.0", &[])],
                    })?;
                    Ok(Response::builder()
                        .header("etag", "\"v1\"")
                        .body(body.into())?)
                }
            }
        });

        let fs = fs::FakeFs::new(cx.executor());
        let cache_dir = Path::new("/extensions/registry-cache");

        let fetch = || fetch_static_index(http_client.as_ref(), fs.as_ref(), cache_dir, URL);

        let index = fetch().await.expect("first fetch downloads the index");
        assert_eq!(index.extensions[0].id.as_ref(), "cached");

        let index = fetch().await.expect("second fetch revalidates");
        assert_eq!(index.extensions[0].id.as_ref(), "cached");

        *server.lock() = Server::Offline;
        let index = fetch()
            .await
            .expect("offline fetch falls back to the cache");
        assert_eq!(index.extensions[0].id.as_ref(), "cached");

        assert_eq!(
            *seen_if_none_match.lock(),
            vec![None, Some("\"v1\"".to_string()), Some("\"v1\"".to_string())]
        );

        let other_url_fails = fetch_static_index(
            http_client.as_ref(),
            fs.as_ref(),
            cache_dir,
            "https://other.example/index.json",
        )
        .await;
        assert!(other_url_fails.is_err(), "no cache exists for another URL");
    }

    #[test]
    fn test_origins_treat_zed_api_as_default() {
        let mut origins = InstallOrigins::default();
        origins.set(
            "a".into(),
            InstallOrigin::Registry {
                index_url: "https://a.example/index.json".into(),
            },
            vec!["lathe".into()],
        );
        assert_eq!(origins.extensions.len(), 1);
        assert_eq!(origins.requires("a"), &[Arc::<str>::from("lathe")]);

        let round_tripped: InstallOrigins =
            serde_json::from_str(&serde_json::to_string(&origins).unwrap()).unwrap();
        assert_eq!(round_tripped, origins);

        origins.set("a".into(), InstallOrigin::ZedApi, vec![]);
        assert!(origins.extensions.is_empty());
        assert_eq!(origins.get("missing"), InstallOrigin::ZedApi);
        assert!(origins.requires("missing").is_empty());
    }

    #[test]
    fn test_validate_git_url() {
        for valid in [
            "https://github.com/example/ext",
            "https://github.com/example/ext.git",
            "ssh://git@github.com/example/ext.git",
            "git@github.com:example/ext.git",
        ] {
            assert!(
                validate_git_url(valid).is_ok(),
                "{valid} should be accepted"
            );
        }
        for invalid in [
            "/Users/me/ext",
            "./ext",
            "file:///Users/me/ext",
            "http://insecure.example/ext",
            "--upload-pack=touch /tmp/x",
            "ext.git",
            "https://github.com/a b",
            "git@:path",
        ] {
            assert!(
                validate_git_url(invalid).is_err(),
                "{invalid} should be rejected"
            );
        }
    }

    #[test]
    fn test_git_checkout_dir_name() {
        let https = git_checkout_dir_name("https://github.com/example/my-ext.git");
        assert!(https.starts_with("my-ext-"), "{https}");
        let scp = git_checkout_dir_name("git@github.com:example/my-ext.git");
        assert!(scp.starts_with("my-ext-"), "{scp}");
        assert_ne!(https, scp, "different URLs get different checkouts");
        assert_eq!(
            git_checkout_dir_name("https://github.com/example/my-ext.git"),
            https,
            "the name is stable"
        );
        assert!(!git_checkout_dir_name("https://x.example/../..").contains(".."));
    }

    #[test]
    fn test_unsupported_requirements_are_skipped() {
        let mut lathe_only = entry("lathe-only", "1.0.0", &[]);
        lathe_only.requires = vec!["lathe".into()];
        let mut future = entry("future", "1.0.0", &[]);
        future.requires = vec!["lathe".into(), "lathe-api-9".into()];
        let mut future_capability = entry("future-capability", "1.0.0", &[]);
        future_capability.lathe_capabilities = vec!["buffer:delete".into()];

        let entries = collect_registry_entries([(
            Arc::from("https://a.example/index.json"),
            StaticIndex {
                schema: 1,
                extensions: vec![
                    lathe_only,
                    future,
                    future_capability,
                    entry("plain", "1.0.0", &[]),
                ],
            },
        )]);

        assert!(entries["lathe-only"].entry.is_lathe_only());
        assert!(!entries["plain"].entry.is_lathe_only());
        assert!(!entries.contains_key("future"));
        assert!(!entries.contains_key("future-capability"));
        assert_eq!(
            unsupported_requirements(&["lathe".into(), "x".into()]),
            vec![Arc::<str>::from("x")]
        );
    }
}
