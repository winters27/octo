//! The live settings: the layered configuration (host keys, appsettings.json, the environment
//! and settings.json), bound into an [`AppSettings`] snapshot that is rebuilt whenever
//! `settings.json` changes.
//!
//! - [`SettingsStore::current`]: the snapshot, read at the point of use;
//! - [`SettingsStore::raw`]: one merged key;
//! - [`SettingsStore::subscribe`]: every new snapshot;
//! - [`SettingsStore::for_tests`] + [`SettingsStore::set`]: a store a test controls.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio::sync::watch;
use tracing::{error, info, warn};

use super::AppSettings;
use super::text::lower_invariant;
use crate::common::layout::caps_to_remember;
use crate::config::{ConfigNode, ConfigTree};
use crate::json::dom::Node;

/// `octo/appsettings.json`, compiled in, so the binary needs no file beside it.
pub const APPSETTINGS_JSON: &str = include_str!("../../../../octo/appsettings.json");

/// `octo/appsettings.Development.json`, layered over it when the environment is Development.
pub const APPSETTINGS_DEVELOPMENT_JSON: &str = include_str!("../../../../octo/appsettings.Development.json");

/// Where settings.json lives in the container.
pub const DEFAULT_SETTINGS_PATH: &str = "/app/config/settings.json";

/// Overrides [`DEFAULT_SETTINGS_PATH`], for running outside the container.
pub const SETTINGS_PATH_ENV: &str = "OCTO_SETTINGS_PATH";

/// The file beside settings.json that remembers the folder caps of every template the settings
/// have held (`SubsonicSettings::layout_caps`), so they outlive an edit and a restart.
pub const LAYOUT_CAPS_FILE: &str = "layout-caps.json";

/// How long a burst of file changes must settle before the file is read again.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);

/// How often the store looks for a settings directory that does not exist yet.
const MISSING_DIR_POLL: Duration = Duration::from_secs(1);

/// The polling watcher's interval, used under `DOTNET_USE_POLLING_FILE_WATCHER` (4 s).
const POLLING_WATCHER_INTERVAL: Duration = Duration::from_secs(4);

/// Raw `Section:Key` reads, so code that needs a raw key (RestartTracker) works on the
/// live store and on a plain tree in tests.
pub trait RawConfig {
    /// The value at a `Section:Key` path, ignoring case. None when absent or null.
    fn raw(&self, key: &str) -> Option<String>;
}

impl RawConfig for ConfigTree {
    fn raw(&self, key: &str) -> Option<String> {
        self.get(key).map(str::to_string)
    }
}

/// The live settings snapshot and the merged configuration it was bound from. Cheap to
/// clone; every clone shares one state.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStore")
            .field("settings_path", &self.inner.settings_path)
            .finish_non_exhaustive()
    }
}

struct Inner {
    /// settings.json; None for a test store.
    settings_path: Option<PathBuf>,
    /// Host keys, appsettings.json, appsettings.{Environment}.json and the environment: fixed
    /// for the life of the process.
    base: ConfigTree,
    /// Whether the snapshot is bound from the configuration. A test store's snapshot is only
    /// ever what `set` gave it, so reloading it does nothing.
    bound_from_config: bool,
    /// `DOTNET_USE_POLLING_FILE_WATCHER`: watch by polling instead of inotify.
    use_polling: bool,
    tree: ArcSwap<ConfigTree>,
    current: ArcSwap<AppSettings>,
    tx: watch::Sender<Arc<AppSettings>>,
    /// Serializes reloads, so two never interleave their reads and stores.
    reload_lock: Mutex<()>,
    /// `layout-caps.json` beside settings.json; None for a test store.
    layout_caps_path: Option<PathBuf>,
    /// The folder caps every template the settings have held gives, beside the built-in ones
    /// (`SubsonicSettings::layout_caps`), and whether the file is behind them.
    layout_caps: Mutex<RememberedCaps>,
    /// The file watcher, kept alive for as long as the store is.
    watcher: Mutex<Option<Box<dyn notify::Watcher + Send>>>,
}

impl SettingsStore {
    /// The settings.json path: `OCTO_SETTINGS_PATH` when set and not blank, else
    /// `/app/config/settings.json`.
    pub fn default_settings_path() -> PathBuf {
        Self::default_settings_path_from(process_env().iter().map(|(k, v)| (k.as_str(), v.as_str())))
    }

    /// [`SettingsStore::default_settings_path`] over a given environment.
    pub fn default_settings_path_from<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> PathBuf {
        env.into_iter()
            .find(|(k, v)| *k == SETTINGS_PATH_ENV && !v.trim().is_empty())
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SETTINGS_PATH))
    }

    /// The production store: the process environment and the given settings.json.
    pub fn load(settings_path: impl Into<PathBuf>) -> SettingsStore {
        Self::from_env(process_env(), Some(settings_path.into()))
    }

    /// A store over an explicit environment (all variables, as `KEY=value` pairs) and an
    /// optional settings.json. Tests use this instead of the process environment.
    pub fn from_env(env: Vec<(String, String)>, settings_path: Option<PathBuf>) -> SettingsStore {
        let use_polling = env.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("DOTNET_USE_POLLING_FILE_WATCHER")
                && (v.eq_ignore_ascii_case("true") || v.trim() == "1")
        });
        let base = base_layers(&env);

        // A settings.json that does not parse does not stop Octo: it keeps running on the
        // other layers, and picks the file up once it is fixed.
        let file_layer = match &settings_path {
            Some(path) => read_file_layer(path).unwrap_or_else(|e| {
                error!(
                    "{} could not be read, so it is ignored until it is fixed: {e:#}",
                    path.display()
                );
                ConfigTree::new()
            }),
            None => ConfigTree::new(),
        };
        let mut tree = base.clone();
        tree.merge(&file_layer);
        let layout_caps_path = settings_path.as_deref().map(layout_caps_path);
        let layout_caps = Mutex::new(RememberedCaps {
            caps: layout_caps_path
                .as_deref()
                .map(read_layout_caps)
                .unwrap_or_default(),
            unsaved: None,
        });
        let mut settings = bind_logged(&tree);
        remember_layout_caps(&layout_caps, layout_caps_path.as_deref(), &mut settings);
        let settings = Arc::new(settings);
        let (tx, _) = watch::channel(settings.clone());

        SettingsStore {
            inner: Arc::new(Inner {
                settings_path,
                base,
                bound_from_config: true,
                use_polling,
                tree: ArcSwap::from_pointee(tree),
                current: ArcSwap::new(settings),
                tx,
                reload_lock: Mutex::new(()),
                layout_caps_path,
                layout_caps,
                watcher: Mutex::new(None),
            }),
        }
    }

    /// A store holding exactly `settings`, with no configuration behind it, for tests: change
    /// it with [`SettingsStore::set`].
    pub fn for_tests(settings: AppSettings) -> SettingsStore {
        let settings = Arc::new(settings);
        let (tx, _) = watch::channel(settings.clone());
        SettingsStore {
            inner: Arc::new(Inner {
                settings_path: None,
                base: ConfigTree::new(),
                bound_from_config: false,
                use_polling: false,
                tree: ArcSwap::from_pointee(ConfigTree::new()),
                current: ArcSwap::new(settings),
                tx,
                reload_lock: Mutex::new(()),
                layout_caps_path: None,
                layout_caps: Mutex::new(RememberedCaps::default()),
                watcher: Mutex::new(None),
            }),
        }
    }

    /// The current snapshot. Read it where the value is used; holding on to it keeps the
    /// values from when it was taken.
    pub fn current(&self) -> Arc<AppSettings> {
        self.inner.current.load_full()
    }

    /// The merged value at a `Section:Key` path, ignoring case. None when absent or null (an
    /// empty object or array in a JSON layer).
    pub fn raw(&self, key: &str) -> Option<String> {
        self.inner.tree.load().raw(key)
    }

    /// One configuration section, as a node tree for [`crate::config::bind`].
    pub fn section_tree(&self, name: &str) -> ConfigNode {
        self.inner.tree.load().section(name)
    }

    /// The whole merged configuration.
    pub fn tree(&self) -> Arc<ConfigTree> {
        self.inner.tree.load_full()
    }

    /// settings.json's path; None for a test store.
    pub fn settings_path(&self) -> Option<&Path> {
        self.inner.settings_path.as_deref()
    }

    /// One line naming `layout-caps.json` and the error, while the caps are not saved; None once
    /// they are. While they are not, each call tries the write once more before it answers (as
    /// each settings reload does), so a fixed folder clears the line at the next look.
    pub fn layout_caps_problem(&self) -> Option<String> {
        let path = self.inner.layout_caps_path.as_deref()?;
        let mut known = self.inner.layout_caps.lock();
        if known.unsaved.is_some() {
            save_layout_caps(&mut known, path);
        }
        let error = known.unsaved.clone()?;
        Some(format!(
            "Octo could not save {}, which remembers how older layouts shortened long folder names: {error}. It tries again each time the settings are read.",
            path.display()
        ))
    }

    /// The receiver sees every new snapshot: on every reload, changed or not, and every `set`.
    pub fn subscribe(&self) -> watch::Receiver<Arc<AppSettings>> {
        self.inner.tx.subscribe()
    }

    /// Replaces the snapshot and tells subscribers. The merged configuration is left as it was.
    pub fn set(&self, settings: AppSettings) {
        let settings = Arc::new(settings);
        self.inner.current.store(settings.clone());
        self.inner.tx.send_replace(settings);
    }

    /// Sets one raw key (`Soulseek:Password`, say). The snapshot is not rebound.
    pub fn set_raw(&self, key: &str, value: Option<&str>) {
        let mut tree = (*self.inner.tree.load_full()).clone();
        tree.set(key, value.map(str::to_string));
        self.inner.tree.store(Arc::new(tree));
    }

    /// Reads settings.json again and rebinds every section. On a file that does not parse, the
    /// last good snapshot and configuration stay in place and the error is logged and
    /// returned. A missing file is not an error: its layer is simply empty.
    pub fn reload_now(&self) -> anyhow::Result<()> {
        Inner::reload(&self.inner)
    }

    /// Watches settings.json and reloads 250 ms after it settles. A directory that does not
    /// exist yet is polled for until it appears, so a file created later is still picked up.
    /// Watching again is a no-op, and so is watching a store with no settings file.
    pub fn start_watching(&self) -> anyhow::Result<()> {
        let Some(path) = self.inner.settings_path.clone() else {
            return Ok(());
        };
        if !self.inner.bound_from_config || self.inner.watcher.lock().is_some() {
            return Ok(());
        }
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("{} names no file", path.display()))?
            .to_os_string();

        let (changes, rx) = mpsc::channel::<()>();
        spawn_debouncer(Arc::downgrade(&self.inner), rx)?;

        if dir.is_dir() {
            let watcher = make_watcher(&dir, file_name, changes, self.inner.use_polling)?;
            *self.inner.watcher.lock() = Some(watcher);
            return Ok(());
        }

        // The directory is not there yet (/app/config before the volume is created). Poll for
        // it, then watch it and read whatever file has appeared.
        info!(
            "{} does not exist yet; settings.json will be read once it does",
            dir.display()
        );
        let weak = Arc::downgrade(&self.inner);
        let use_polling = self.inner.use_polling;
        std::thread::Builder::new()
            .name("settings-dir-poll".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(MISSING_DIR_POLL);
                    let Some(inner) = weak.upgrade() else { return };
                    if !dir.is_dir() {
                        continue;
                    }
                    match make_watcher(&dir, file_name, changes.clone(), use_polling) {
                        Ok(watcher) => *inner.watcher.lock() = Some(watcher),
                        Err(e) => error!("Could not watch {}: {e:#}", dir.display()),
                    }
                    let _ = changes.send(());
                    return;
                }
            })?;
        Ok(())
    }
}

impl RawConfig for SettingsStore {
    fn raw(&self, key: &str) -> Option<String> {
        SettingsStore::raw(self, key)
    }
}

impl Inner {
    fn reload(inner: &Arc<Inner>) -> anyhow::Result<()> {
        let Some(path) = inner.settings_path.as_deref().filter(|_| inner.bound_from_config) else {
            return Ok(());
        };
        let _guard = inner.reload_lock.lock();
        let file_layer = match read_file_layer(path) {
            Ok(layer) => layer,
            Err(e) => {
                error!(
                    "{} could not be read; keeping the settings from before the change: {e:#}",
                    path.display()
                );
                return Err(e);
            }
        };
        let mut tree = inner.base.clone();
        tree.merge(&file_layer);
        let mut settings = bind_logged(&tree);
        remember_layout_caps(
            &inner.layout_caps,
            inner.layout_caps_path.as_deref(),
            &mut settings,
        );
        let settings = Arc::new(settings);
        inner.tree.store(Arc::new(tree));
        inner.current.store(settings.clone());
        inner.tx.send_replace(settings);
        Ok(())
    }
}

/// Binds every section and logs what did not convert.
fn bind_logged(tree: &ConfigTree) -> AppSettings {
    let (settings, warnings) = AppSettings::bind(tree);
    for w in warnings {
        warn!(path = %w.path, "Setting {} could not be used, so a lower source or the default applies: {}", w.path, w.message);
    }
    settings
}

/// `layout-caps.json` beside `settings_path`.
fn layout_caps_path(settings_path: &Path) -> PathBuf {
    settings_path.parent().map_or_else(
        || PathBuf::from(LAYOUT_CAPS_FILE),
        |dir| dir.join(LAYOUT_CAPS_FILE),
    )
}

/// The caps `layout-caps.json` remembers, a JSON list of numbers. A missing file remembers
/// none, and so does one that does not read, with a warning: the next cap written replaces it.
fn read_layout_caps(path: &Path) -> BTreeSet<usize> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeSet::new(),
        Err(e) => {
            warn!(
                "{} could not be read, so older layouts' folder caps are not known: {e}",
                path.display()
            );
            return BTreeSet::new();
        }
    };
    match serde_json::from_str::<Vec<usize>>(&text) {
        Ok(caps) => caps.into_iter().collect(),
        Err(e) => {
            warn!(
                "{} is not a list of numbers, so older layouts' folder caps are not known: {e}",
                path.display()
            );
            BTreeSet::new()
        }
    }
}

/// Writes `caps` to `path` as a sorted JSON list, through a temporary file renamed into place
/// (removed again when the rename fails).
fn write_layout_caps(path: &Path, caps: &BTreeSet<usize>) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let list: Vec<String> = caps.iter().map(usize::to_string).collect();
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, format!("[{}]\n", list.join(", ")))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// The caps remembered in `layout-caps.json`, beside the built-in ones.
#[derive(Default)]
struct RememberedCaps {
    caps: BTreeSet<usize>,
    /// Why the last write of `caps` failed: Some while the file is behind them, and each
    /// settings reload writes it again until a write succeeds.
    unsaved: Option<String>,
}

/// Adds the caps of the three templates in `settings` to the `known` ones, writes them to `path`
/// when they grew or an earlier write failed, and gives `settings` every known cap
/// (`SubsonicSettings::layout_caps`).
fn remember_layout_caps(known: &Mutex<RememberedCaps>, path: Option<&Path>, settings: &mut AppSettings) {
    let subsonic = &mut settings.subsonic;
    let mut known = known.lock();
    let before = known.caps.len();
    known.caps.extend(caps_to_remember([
        subsonic.folder_template.as_str(),
        subsonic.folder_template_singles.as_str(),
        subsonic.folder_template_compilations.as_str(),
    ]));
    if let Some(path) = path
        && (known.caps.len() > before || known.unsaved.is_some())
    {
        save_layout_caps(&mut known, path);
    }
    subsonic.layout_caps = known.caps.iter().copied().collect();
}

/// Writes the known caps to `path` once: a success clears why the last write failed, and a
/// failure keeps why, logged when the reason is new.
fn save_layout_caps(known: &mut RememberedCaps, path: &Path) {
    match write_layout_caps(path, &known.caps) {
        Ok(()) => {
            if known.unsaved.take().is_some() {
                info!("{} is written now", path.display());
            }
        }
        Err(e) => {
            let error = e.to_string();
            if known.unsaved.as_deref() != Some(error.as_str()) {
                warn!(
                    "Could not write {}: {error}. Octo tries again each time the settings are read.",
                    path.display()
                );
            }
            known.unsaved = Some(error);
        }
    }
}

/// The process environment, with names and values that are not UTF-8 read lossily.
fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect()
}

/// Everything below settings.json, lowest first: `DOTNET_*` then `ASPNETCORE_*` variables with
/// the prefix removed (host keys such as `environment` and `urls`), appsettings.json,
/// appsettings.{Environment}.json, then every environment variable with `__` meaning `:`.
fn base_layers(env: &[(String, String)]) -> ConfigTree {
    let prefixed = |prefix: &str| {
        ConfigTree::from_env_vars(env.iter().filter_map(|(k, v)| {
            let head = k.get(..prefix.len())?;
            head.eq_ignore_ascii_case(prefix)
                .then(|| (k[prefix.len()..].to_string(), v.clone()))
        }))
    };
    let mut tree = prefixed("DOTNET_");
    tree.merge(&prefixed("ASPNETCORE_"));

    tree.merge(&embedded_json("appsettings.json", APPSETTINGS_JSON));
    let environment = tree.get("environment").unwrap_or("Production").to_string();
    if environment.eq_ignore_ascii_case("Development") {
        tree.merge(&embedded_json(
            "appsettings.Development.json",
            APPSETTINGS_DEVELOPMENT_JSON,
        ));
    }

    warn_case_only_duplicates(env);
    tree.merge(&ConfigTree::from_env_vars(
        env.iter().map(|(k, v)| (k.as_str(), v.clone())),
    ));
    tree
}

fn embedded_json(name: &str, text: &str) -> ConfigTree {
    match parse_config_json(text) {
        Ok(tree) => tree,
        Err(e) => {
            // Compiled in and checked by a test, so this cannot happen in a release.
            error!("The built-in {name} does not parse: {e:#}");
            ConfigTree::new()
        }
    }
}

/// Two variables whose names differ only in case land on one key. The later one in the
/// environment wins, with a warning.
fn warn_case_only_duplicates(env: &[(String, String)]) {
    let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for (k, _) in env {
        let key = k.replace("__", ":");
        if let Some(first) = seen.insert(lower_invariant(&key), k.as_str())
            && first != k
        {
            warn!("Environment variables {first} and {k} name the same setting; {k} is used");
        }
    }
}

/// The settings.json layer. A missing file is an empty layer; content that does not parse is
/// an error.
fn read_file_layer(path: &Path) -> anyhow::Result<ConfigTree> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            parse_config_json(text.strip_prefix('\u{feff}').unwrap_or(&text))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigTree::new()),
        Err(e) => Err(e.into()),
    }
}

/// Flattens a JSON configuration file as JsonConfigurationFileParser does: comments and
/// trailing commas are allowed, the top level must be an object, numbers keep their text, a
/// boolean is `True`/`False`, null is `""`, and an empty object or array is the key with a null
/// value. Two keys that differ only in case are an error ("A duplicate key ... was found").
fn parse_config_json(text: &str) -> anyhow::Result<ConfigTree> {
    let node = Node::parse(text).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))?;
    let Node::Object(_) = &node else {
        anyhow::bail!("the top-level JSON element must be an object");
    };
    let mut tree = ConfigTree::new();
    flatten(&mut tree, "", &node)?;
    Ok(tree)
}

fn flatten(tree: &mut ConfigTree, prefix: &str, node: &Node) -> anyhow::Result<()> {
    let child = |segment: &str| {
        if prefix.is_empty() {
            segment.to_string()
        } else {
            format!("{prefix}:{segment}")
        }
    };
    let leaf = |tree: &mut ConfigTree, value: Option<String>| {
        if tree.contains(prefix) {
            anyhow::bail!("A duplicate key '{prefix}' was found.");
        }
        tree.set(prefix, value);
        Ok(())
    };
    match node {
        Node::Object(map) => {
            if map.is_empty() && !prefix.is_empty() {
                leaf(tree, None)?;
            }
            for (k, v) in map {
                flatten(tree, &child(k), v)?;
            }
        }
        Node::Array(items) => {
            if items.is_empty() && !prefix.is_empty() {
                leaf(tree, None)?;
            }
            for (i, v) in items.iter().enumerate() {
                flatten(tree, &child(&i.to_string()), v)?;
            }
        }
        Node::Null => leaf(tree, Some(String::new()))?,
        Node::Bool(b) => leaf(tree, Some(if *b { "True" } else { "False" }.to_string()))?,
        Node::Number(n) => leaf(tree, Some(n.clone()))?,
        Node::String(s) => leaf(tree, Some(s.clone()))?,
    }
    Ok(())
}

/// Reloads after a burst of change notifications has been quiet for [`RELOAD_DEBOUNCE`].
/// Stops when the store is dropped (the watcher, and with it the sender, goes away).
fn spawn_debouncer(weak: Weak<Inner>, rx: mpsc::Receiver<()>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("settings-reload".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(RELOAD_DEBOUNCE) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                let Some(inner) = weak.upgrade() else { return };
                // Errors are logged inside; the last good snapshot stays.
                let _ = Inner::reload(&inner);
            }
        })?;
    Ok(())
}

fn make_watcher(
    dir: &Path,
    file_name: std::ffi::OsString,
    changes: mpsc::Sender<()>,
    use_polling: bool,
) -> anyhow::Result<Box<dyn notify::Watcher + Send>> {
    use notify::event::{AccessKind, AccessMode};
    use notify::{EventKind, RecursiveMode, Watcher};

    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Reading the file (our own reload) must not count as a change.
        let is_change = match event.kind {
            EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
            EventKind::Access(_) => false,
            _ => true,
        };
        // The writer's rename names settings.json as its destination, so it counts.
        if is_change
            && event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(file_name.as_os_str()))
        {
            let _ = changes.send(());
        }
    };
    let mut watcher: Box<dyn Watcher + Send> = if use_polling {
        Box::new(notify::PollWatcher::new(
            handler,
            notify::Config::default().with_poll_interval(POLLING_WATCHER_INTERVAL),
        )?)
    } else {
        Box::new(notify::recommended_watcher(handler)?)
    };
    watcher.watch(dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
