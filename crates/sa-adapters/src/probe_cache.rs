//! Reusable results of model/capability probes. Credentials and launch
//! configuration stay with the adapter and are never serialized here.

use crate::{common::Emit, custom, sys::Sys};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, LazyLock, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering::Relaxed},
};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

pub(crate) const FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);
const VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProbeKey {
    agent: String,
    digest: String,
}

pub(crate) fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

pub(crate) type FileStamp = (u64, String);

pub(crate) fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let nanos = match meta.modified().ok()?.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(time) => time.as_nanos().to_string(),
        Err(time) => format!("-{}", time.duration().as_nanos()),
    };
    Some((meta.len(), nanos))
}

pub(crate) fn config_digest(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

impl ProbeKey {
    pub(crate) fn scoped(&self, inputs: &Value) -> Self {
        Self {
            agent: self.agent.clone(),
            digest: digest(&json!([self.digest, inputs])),
        }
    }

    pub(crate) fn new(agent: &str, sys: &Sys, exe: Option<&Path>, inputs: Value) -> Self {
        // The child inherits the whole environment, including configurable
        // env_key/env_http_headers names. Persist only the combined digest.
        let mut vars = sys.vars().to_vec();
        if cfg!(windows) {
            for (name, _) in &mut vars {
                name.make_ascii_uppercase();
            }
        }
        vars.sort_by(|(a, _), (b, _)| a.cmp(b));
        Self {
            agent: agent.into(),
            digest: digest(&json!([
                sys.home,
                sys.temp,
                exe,
                exe.and_then(file_stamp),
                vars,
                inputs,
            ])),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    instant: Instant,
    unix_ms: u64,
    loaded_age: Option<Duration>,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Stamp {
    pub(crate) fn now() -> Self {
        Self {
            instant: Instant::now(),
            unix_ms: unix_ms(),
            loaded_age: None,
        }
    }

    fn load(unix_ms: u64, now: Self) -> Option<Self> {
        let age = Duration::from_millis(now.unix_ms.checked_sub(unix_ms)?);
        (age < FRESHNESS).then_some(())?;
        Some(Self {
            instant: now.instant,
            unix_ms,
            loaded_age: Some(age),
        })
    }

    fn age(self) -> Duration {
        self.loaded_age.unwrap_or_default() + self.instant.elapsed()
    }

    fn newer_than(self, other: Self) -> bool {
        match (self.loaded_age, other.loaded_age) {
            (Some(_), Some(_)) => self.unix_ms > other.unix_ms,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => self.instant > other.instant,
        }
    }
}

struct Fetched {
    // A failed probe retains its ordering marker without offering a hit.
    value: Option<Value>,
    started: Stamp,
}

impl Fetched {
    fn invalidate_through(&mut self, cutoff: Stamp) -> bool {
        if !self.started.newer_than(cutoff) && self.value.take().is_some() {
            self.started = cutoff;
            return true;
        }
        false
    }
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    key: ProbeKey,
    slot: String,
    started_unix_ms: u64,
    value: Option<Value>,
}

#[derive(Deserialize)]
struct DiskCache {
    version: u32,
    entries: Vec<Value>,
}

type Slots = HashMap<String, Fetched>;

struct State {
    sources: HashMap<ProbeKey, Slots>,
    freshness: Duration,
    revision: u64,
}

impl State {
    fn open(file: Option<&Path>) -> Self {
        let now = Stamp::now();
        let sources = file
            .and_then(|path| std::fs::File::open(path).ok())
            .and_then(|file| {
                serde_json::from_reader::<_, DiskCache>(std::io::BufReader::new(file)).ok()
            })
            .filter(|file| file.version == VERSION)
            .into_iter()
            .flat_map(|file| file.entries)
            .filter_map(|entry| serde_json::from_value::<FileEntry>(entry).ok())
            .filter(|entry| matches!(entry.key.agent.as_str(), "antigravity" | "claude" | "codex"))
            .filter_map(|entry| {
                let started = Stamp::load(entry.started_unix_ms, now)?;
                Some((
                    entry.key,
                    entry.slot,
                    Fetched {
                        value: entry.value,
                        started,
                    },
                ))
            })
            .fold(
                HashMap::<ProbeKey, Slots>::new(),
                |mut map, (key, slot, fetched)| {
                    let slots = map.entry(key).or_default();
                    if slots
                        .get(&slot)
                        .is_none_or(|old| !old.started.newer_than(fetched.started))
                    {
                        slots.insert(slot, fetched);
                    }
                    map
                },
            );
        Self {
            sources,
            freshness: FRESHNESS,
            revision: 0,
        }
    }

    fn changed(&mut self) {
        let freshness = self.freshness;
        self.sources.retain(|_, slots| {
            slots.retain(|_, item| item.started.age() < freshness);
            !slots.is_empty()
        });
        self.revision += 1;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            version: VERSION,
            entries: self
                .sources
                .iter()
                .flat_map(|(key, slots)| {
                    slots.iter().map(move |(slot, entry)| FileEntry {
                        key: key.clone(),
                        slot: slot.clone(),
                        started_unix_ms: entry.started.unix_ms,
                        value: entry.value.clone(),
                    })
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct Snapshot {
    version: u32,
    entries: Vec<FileEntry>,
}

fn persist(path: &Path, snapshot: &Snapshot) -> anyhow::Result<()> {
    let dir = path.parent().expect("cache file has a parent");
    std::fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(temp.as_file_mut(), snapshot)?;
    temp.persist(path)?;
    Ok(())
}

#[derive(Default)]
struct SharedCache {
    file: Option<PathBuf>,
    state: OnceLock<Mutex<State>>,
    writer: Mutex<WriteProgress>,
    writer_ready: Condvar,
}

#[derive(Default)]
struct WriteProgress {
    written_revision: u64,
    active: bool,
}

struct ActiveWriter<'a> {
    cache: &'a SharedCache,
    published: Option<u64>,
}

impl Drop for ActiveWriter<'_> {
    fn drop(&mut self) {
        let mut writer = self.cache.writer.lock().unwrap();
        if let Some(revision) = self.published {
            writer.written_revision = revision;
        }
        writer.active = false;
        self.cache.writer_ready.notify_all();
    }
}

/// A data directory shares one in-memory cache and one disk writer.
#[derive(Clone)]
pub(crate) struct ProbeCache(Arc<SharedCache>);

type OpenCaches = HashMap<PathBuf, Weak<SharedCache>>;
static OPEN: LazyLock<Mutex<OpenCaches>> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl ProbeCache {
    pub(crate) fn new(data_dir: Option<&Path>) -> Self {
        let Some(dir) = data_dir else {
            return Self(Arc::default());
        };
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        let file = dir.join("cache/probes-v1.json");
        let shared = {
            let mut open = OPEN.lock().unwrap();
            open.retain(|_, instance| instance.strong_count() > 0);
            if let Some(shared) = open.get(&file).and_then(Weak::upgrade) {
                shared
            } else {
                let shared = Arc::new(SharedCache {
                    file: Some(file.clone()),
                    ..Default::default()
                });
                open.insert(file, Arc::downgrade(&shared));
                shared
            }
        };
        let cache = Self(shared);
        cache.state();
        cache
    }

    fn state(&self) -> &Mutex<State> {
        self.0
            .state
            .get_or_init(|| Mutex::new(State::open(self.0.file.as_deref())))
    }

    pub(crate) fn flush(&self) {
        self.flush_with(persist);
    }

    fn flush_with(&self, write: impl FnOnce(&Path, &Snapshot) -> anyhow::Result<()>) {
        let Some(file) = self.0.file.as_deref() else {
            return;
        };
        // Snapshot only after reserving the disk writer. Queued flushes share
        // the latest memory revision, so a slower writer cannot restore old data.
        let mut writer = self
            .0
            .writer_ready
            .wait_while(self.0.writer.lock().unwrap(), |writer| writer.active)
            .unwrap();
        let (revision, snapshot) = {
            let state = self.state().lock().unwrap();
            if state.revision == writer.written_revision {
                return;
            }
            (state.revision, state.snapshot())
        };
        writer.active = true;
        drop(writer);
        let mut reservation = ActiveWriter {
            cache: &self.0,
            published: None,
        };
        if write(file, &snapshot).is_ok() {
            reservation.published = Some(revision);
        }
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, key: &ProbeKey, slot: &str) -> Option<T> {
        self.read_stamped(key, slot).map(|(value, _)| value)
    }

    fn read_stamped<T: DeserializeOwned>(&self, key: &ProbeKey, slot: &str) -> Option<(T, Stamp)> {
        let (value, started) = {
            let state = self.state().lock().unwrap();
            let item = state.sources.get(key)?.get(slot)?;
            if item.started.age() >= state.freshness {
                return None;
            }
            (item.value.clone()?, item.started)
        };
        Some((serde_json::from_value(value).ok()?, started))
    }

    pub(crate) fn record<T: Serialize>(
        &self,
        key: &ProbeKey,
        slot: &str,
        started: Stamp,
        value: Option<&T>,
    ) {
        self.replace(key, slot, started, value, |_| true);
    }

    /// Updating a model list can invalidate results for removed models.
    pub(crate) fn replace<T: Serialize>(
        &self,
        key: &ProbeKey,
        slot: &str,
        started: Stamp,
        value: Option<&T>,
        keep_other: impl Fn(&str) -> bool,
    ) {
        let value = value.and_then(|v| serde_json::to_value(v).ok());
        let mut state = self.state().lock().unwrap();
        let slots = state.sources.entry(key.clone()).or_default();
        if slots
            .get(slot)
            .is_some_and(|old| old.started.newer_than(started))
        {
            return;
        }
        for (name, fetched) in slots.iter_mut() {
            if name != slot && !keep_other(name) {
                fetched.invalidate_through(started);
            }
        }
        slots.insert(slot.into(), Fetched { value, started });
        state.changed();
    }

    pub(crate) fn evict_through(&self, key: &ProbeKey, cutoff: Stamp) {
        let mut state = self.state().lock().unwrap();
        if let Some(slots) = state.sources.get_mut(key) {
            for entry in slots.values_mut() {
                entry.invalidate_through(cutoff);
            }
        }
        state.changed();
    }

    pub(crate) fn evict_used(&self, key: &ProbeKey, used: &[ProbeUse]) {
        let changed = {
            let mut state = self.state().lock().unwrap();
            let Some(slots) = state.sources.get_mut(key) else {
                return;
            };
            let mut changed = false;
            for item in used {
                if let Some(entry) = slots.get_mut(&item.slot) {
                    changed |= entry.invalidate_through(item.started);
                }
            }
            if changed {
                state.changed();
            }
            changed
        };
        if changed {
            self.flush();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_freshness(&self, freshness: Duration) {
        self.state().lock().unwrap().freshness = freshness;
    }
}

/// Track which work a single discovery attempt actually performed.
pub(crate) struct Probes<'a> {
    cache: &'a ProbeCache,
    key: &'a ProbeKey,
    reuse: bool,
    hit: AtomicBool,
    probed: AtomicBool,
    observed: Mutex<HashMap<String, Stamp>>,
}

pub(crate) struct ProbeUse {
    slot: String,
    started: Stamp,
}

pub(crate) type ModelProbeResult = (Vec<Value>, Option<String>);

impl<'a> Probes<'a> {
    pub(crate) fn new(cache: &'a ProbeCache, key: &'a ProbeKey, reuse: bool) -> Self {
        Self {
            cache,
            key,
            reuse,
            hit: false.into(),
            probed: false.into(),
            observed: Mutex::default(),
        }
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, slot: &str) -> Option<T> {
        let (value, started) = self
            .reuse
            .then(|| self.cache.read_stamped(self.key, slot))
            .flatten()?;
        self.observed.lock().unwrap().insert(slot.into(), started);
        self.hit.store(true, Relaxed);
        Some(value)
    }

    pub(crate) fn record<T: Serialize>(&self, slot: &str, started: Stamp, value: Option<&T>) {
        self.cache.record(self.key, slot, started, value);
        if value.is_some() {
            self.observed.lock().unwrap().insert(slot.into(), started);
        }
    }

    pub(crate) fn observed(&self, slots: &[&str]) -> Vec<ProbeUse> {
        let observed = self.observed.lock().unwrap();
        slots
            .iter()
            .filter_map(|&slot| {
                Some(ProbeUse {
                    slot: slot.into(),
                    started: *observed.get(slot)?,
                })
            })
            .collect()
    }

    pub(crate) fn flush(&self) {
        if self.probed.load(Relaxed) {
            self.cache.flush();
        }
    }

    pub(crate) fn used_cache(&self) -> bool {
        self.hit.load(Relaxed)
    }

    pub(crate) fn start(&self) -> Stamp {
        self.probed.store(true, Relaxed);
        Stamp::now()
    }

    pub(crate) fn reused_all(&self) -> bool {
        self.used_cache() && !self.probed.load(Relaxed)
    }

    pub(crate) async fn fetch_models(
        &self,
        slot: &str,
        cancel: &CancellationToken,
        probe: impl Future<Output = anyhow::Result<ModelProbeResult>>,
    ) -> anyhow::Result<ModelProbeResult> {
        if let Some(models) = self.read(slot) {
            return Ok((models, None));
        }
        let started = self.start();
        let result = probe.await;
        if !cancel.is_cancelled() {
            let models = result
                .as_ref()
                .ok()
                .filter(|(models, error)| !models.is_empty() && error.is_none())
                .map(|(models, _)| safe_models(&json!(models)));
            self.record(slot, started, models.as_ref());
        }
        result
    }
}

pub(crate) fn probe_slot(kind: &str, identity: &Value) -> String {
    format!("{kind}:{}", digest(identity))
}

pub(crate) fn custom_probe_slot(id: &str) -> String {
    probe_slot("custom", &json!(id))
}

/// Reapply only the remote model list to the freshly built custom rows,
/// so names, endpoint, credentials and other launch inputs remain live.
pub(crate) async fn refresh_custom(
    catalog: &mut Value,
    customs: &[custom::CustomProvider],
    sys: &Sys,
    cancel: &CancellationToken,
    probes: &Probes<'_>,
) {
    if customs.is_empty() {
        return;
    }
    let mut cached = Vec::new();
    let mut missed = Vec::new();
    for provider in customs {
        let id = provider.route_id();
        if catalog["providers"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == id))
        {
            continue;
        }
        let slot = custom_probe_slot(&id);
        if let Some(models) = probes.read::<Vec<Value>>(&slot) {
            cached.push((id, models));
        } else {
            missed.push(provider.clone());
        }
    }
    if !missed.is_empty() {
        let started = probes.start();
        // Keep native model metadata as the sole input to custom supplementation.
        custom::refresh(catalog, &missed, sys, cancel).await;
        if !cancel.is_cancelled() {
            for provider in missed {
                let id = provider.route_id();
                let Some(row) = custom_row(catalog, &id) else {
                    continue;
                };
                let models = (row["catalog_complete"] == true).then(|| safe_models(&row["models"]));
                probes.record(&custom_probe_slot(&id), started, models.as_ref());
            }
        }
    }
    custom::merge(catalog, customs, sys);
    for (id, models) in cached {
        if let Some(row) = custom_row(catalog, &id) {
            row["models"] = json!(models);
            row["available"] = json!(true);
            row["catalog_complete"] = json!(true);
            row["detail"] = json!("供应商在线完整模型目录");
        }
    }
}

fn custom_row<'a>(catalog: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    catalog["providers"]
        .as_array_mut()?
        .iter_mut()
        .find(|row| row["id"] == id && row["custom"] == true)
}

/// Return only the model ids whose effort options came from shared metadata.
pub(crate) fn supplement_efforts(rows: &mut [Value], metadata: &[Value]) -> HashSet<String> {
    rows.iter_mut()
        .filter_map(|row| {
            if row.get("reasoningEfforts").is_some() {
                return None;
            }
            crate::catalog::supplement(std::slice::from_mut(row), metadata);
            row.get("reasoningEfforts")?;
            row["id"].as_str().map(str::to_owned)
        })
        .collect()
}

/// Retain the model data used by routing and effort selection. Discovery
/// diagnostics, provider labels and arbitrary nested metadata stay out of disk.
pub(crate) fn safe_models(models: &Value) -> Vec<Value> {
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row["id"].as_str()?;
            let mut result = json!({"id": id, "label": row["label"].as_str().unwrap_or(id)});
            for name in ["resolvedModel", "defaultReasoningEffort"] {
                if let Some(value) = row[name].as_str() {
                    result[name] = json!(value);
                }
            }
            if let Some(levels) = row["reasoningEfforts"].as_array() {
                result["reasoningEfforts"] =
                    json!(levels.iter().filter_map(Value::as_str).collect::<Vec<_>>());
            }
            if let Some(mapping) = row["reasoningEffortMapping"].as_object() {
                result["reasoningEffortMapping"] = Value::Object(
                    mapping
                        .iter()
                        .filter(|(_, value)| value.is_string())
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                );
            }
            Some(result)
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(crate) struct DiscoveryTiming {
    ms: u128,
    cache: &'static str,
}

impl DiscoveryTiming {
    pub(crate) fn new(started: Instant, hit: bool, refreshed: bool) -> Self {
        let cache = if refreshed {
            "refresh"
        } else if hit {
            "hit"
        } else {
            "miss"
        };
        Self {
            ms: started.elapsed().as_millis(),
            cache,
        }
    }

    pub(crate) fn spawned(self, before: Instant) -> TurnTiming {
        let started = Instant::now();
        TurnTiming {
            discovery: self,
            started,
            spawn_ms: started.duration_since(before).as_millis(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TurnTiming {
    discovery: DiscoveryTiming,
    started: Instant,
    spawn_ms: u128,
}

impl TurnTiming {
    pub(crate) fn emit(self, emit: &Emit) {
        emit.journal(
            "turn.timing",
            json!({
                "discover_ms": self.discovery.ms,
                "spawn_ms": self.spawn_ms,
                "handshake_ms": self.started.elapsed().as_millis(),
                "cache": self.discovery.cache,
            }),
        );
    }
}

#[cfg(test)]
pub(crate) fn assert_timing(events: &[sa_engine::adapter::AdapterEvent], cache: &str) {
    use sa_engine::adapter::AdapterEvent;
    let mut timings = events.iter().filter_map(|event| match event {
        AdapterEvent::Journal { kind, data } if kind == "turn.timing" => Some(data),
        _ => None,
    });
    let timing = timings.next().expect("turn.timing");
    assert!(timings.next().is_none());
    assert_eq!(timing["cache"], cache);
    for field in ["discover_ms", "spawn_ms", "handshake_ms"] {
        assert!(timing[field].is_u64(), "{field}: {timing}");
    }
}

#[cfg(test)]
#[path = "../tests/fixtures/startup_cache.rs"]
mod startup_cache;

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> ProbeKey {
        ProbeKey {
            agent: "antigravity".into(),
            digest: source.into(),
        }
    }

    #[test]
    fn older_results_and_failures_cannot_replace_newer_probe_results() {
        let cache = ProbeCache::new(None);
        let key = key("source");
        let now = Stamp::now();
        let older = Stamp::load(now.unix_ms - 1000, now).unwrap();
        cache.record(&key, "models", now, Some(&vec!["new"]));
        cache.record(&key, "new", now, Some(&"effort"));
        cache.replace(&key, "models", older, Some(&vec!["old"]), |_| false);
        cache.replace::<Vec<String>>(&key, "models", older, None, |_| false);
        cache.record::<String>(&key, "new", older, None);
        assert_eq!(cache.read::<Vec<String>>(&key, "models").unwrap(), ["new"]);
        assert_eq!(cache.read::<String>(&key, "new").as_deref(), Some("effort"));
        cache.replace::<Vec<String>>(&key, "models", now, None, |_| false);
        assert!(cache.read::<Vec<String>>(&key, "models").is_none());
        assert!(cache.read::<String>(&key, "new").is_none());
    }

    #[test]
    fn newer_failure_markers_reject_late_successes() {
        let cache = ProbeCache::new(None);
        let key = key("source");
        let now = Stamp::now();
        let older = Stamp::load(now.unix_ms - 1000, now).unwrap();
        for slot in ["models", "effort"] {
            cache.record::<String>(&key, slot, now, None);
            cache.record(&key, slot, older, Some(&"late"));
            assert!(cache.read::<String>(&key, slot).is_none());
            cache.record(&key, slot, Stamp::now(), Some(&"retried"));
            assert_eq!(cache.read::<String>(&key, slot).as_deref(), Some("retried"));
        }
    }

    #[test]
    fn writes_prune_expired_keys_including_failure_markers() {
        let cache = ProbeCache::new(None);
        let old = key("old");
        let failed = key("failed");
        let current = key("current");
        let now = Stamp::now();
        let expired = Stamp::load(now.unix_ms - 2000, now).unwrap();
        cache.record(&old, "models", expired, Some(&"value"));
        cache.record::<String>(&failed, "models", expired, None);
        cache.record::<String>(&failed, "effort", expired, None);
        assert_eq!(cache.state().lock().unwrap().sources.len(), 2);
        cache.set_freshness(Duration::from_secs(1));
        cache.record::<String>(&current, "models", now, None);
        assert_eq!(cache.state().lock().unwrap().sources.len(), 1);
        assert!(cache.state().lock().unwrap().sources.contains_key(&current));
    }

    #[test]
    fn persisted_entries_ignore_unknown_adapters_and_future_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let key = key("source");
        {
            let cache = ProbeCache::new(Some(dir.path()));
            cache.record(&key, "models", Stamp::now(), Some(&"valid"));
            cache.flush();
        }
        let path = dir.path().join("cache/probes-v1.json");
        let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entries = disk["entries"].as_array_mut().unwrap();
        let mut unknown = entries[0].clone();
        unknown["key"]["agent"] = json!("unknown");
        let mut future = entries[0].clone();
        future["slot"] = json!("future");
        future["started_unix_ms"] = json!(unix_ms() + 60_000);
        entries.extend([unknown, future]);
        std::fs::write(&path, disk.to_string()).unwrap();
        let cache = ProbeCache::new(Some(dir.path()));
        assert_eq!(
            cache.read::<String>(&key, "models").as_deref(),
            Some("valid")
        );
        assert!(cache.read::<String>(&key, "future").is_none());
        cache.record(&key, "models", Stamp::now(), Some(&"updated"));
        cache.flush();
        let disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn versioned_model_ids_survive_disk_round_trip_without_account_fields() {
        let dir = tempfile::tempdir().unwrap();
        let key = key("versioned-model");
        let id = "model@20250101";
        let rows = safe_models(&json!([{
            "id": id, "label": id, "resolvedModel": "base@20250101",
            "email": "account@example.com", "account": {"token": "private"}
        }]));
        {
            let cache = ProbeCache::new(Some(dir.path()));
            cache.record(&key, "models", Stamp::now(), Some(&rows));
            cache.flush();
        }
        let cache = ProbeCache::new(Some(dir.path()));
        assert_eq!(
            cache.read::<Vec<Value>>(&key, "models"),
            Some(vec![
                json!({"id": id, "label": id, "resolvedModel": "base@20250101"})
            ]),
        );
    }

    #[test]
    fn eviction_preserves_unrelated_slots_and_newer_results() {
        let cache = ProbeCache::new(None);
        let key = key("all-providers");
        let now = Stamp::now();
        let old = Stamp::load(now.unix_ms - 1000, now).unwrap();
        for slot in ["a", "b"] {
            cache.record(&key, slot, old, Some(&"cached"));
        }
        let probes = Probes::new(&cache, &key, true);
        assert_eq!(probes.read::<String>("a").as_deref(), Some("cached"));
        assert_eq!(probes.read::<String>("b").as_deref(), Some("cached"));
        let used = probes.observed(&["a"]);
        cache.record(&key, "a", now, Some(&"newer"));
        cache.evict_used(&key, &used);
        assert_eq!(cache.read::<String>(&key, "a").as_deref(), Some("newer"));

        assert_eq!(probes.read::<String>("a").as_deref(), Some("newer"));
        cache.evict_used(&key, &probes.observed(&["a"]));
        assert!(cache.read::<String>(&key, "a").is_none());
        assert_eq!(cache.read::<String>(&key, "b").as_deref(), Some("cached"));
    }

    #[test]
    fn persistence_batches_records_and_coalesces_writers_without_locking_readers() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let key = key("many-models");
        let cache = ProbeCache::new(Some(dir.path()));
        let path = dir.path().join("cache/probes-v1.json");
        for slot in ["models", "effort-a", "effort-b"] {
            cache.record(&key, slot, Stamp::now(), Some(&"initial"));
        }
        assert!(!path.exists());

        let (writing, in_flight) = mpsc::channel();
        let (release, paused) = mpsc::channel();
        std::thread::scope(|tasks| {
            let writer_cache = &cache;
            let writer = tasks.spawn(move || {
                writer_cache.flush_with(|path, snapshot| {
                    assert_eq!(snapshot.entries.len(), 3);
                    writing.send(()).unwrap();
                    paused.recv_timeout(Duration::from_secs(5)).unwrap();
                    persist(path, snapshot)
                })
            });
            in_flight.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(cache.state().try_lock().is_ok());
            assert!(cache.0.writer.try_lock().is_ok());
            assert_eq!(
                cache.read::<String>(&key, "models").as_deref(),
                Some("initial")
            );
            cache.record(&key, "models", Stamp::now(), Some(&"latest"));

            let count = AtomicUsize::new(0);
            std::thread::scope(|writers| {
                for _ in 0..2 {
                    writers.spawn(|| {
                        cache.flush_with(|path, snapshot| {
                            count.fetch_add(1, Relaxed);
                            persist(path, snapshot)
                        })
                    });
                }
                release.send(()).unwrap();
            });
            writer.join().unwrap();
            assert_eq!(count.load(Relaxed), 1);
        });
        drop(cache);
        let reopened = ProbeCache::new(Some(dir.path()));
        assert_eq!(
            reopened.read::<String>(&key, "models").as_deref(),
            Some("latest")
        );
        assert_eq!(
            reopened.read::<String>(&key, "effort-a").as_deref(),
            Some("initial")
        );
    }

    #[test]
    fn cache_hit_timing_includes_elapsed_discovery() {
        let start = Instant::now() - Duration::from_millis(25);
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        DiscoveryTiming::new(start, true, false)
            .spawned(Instant::now())
            .emit(&Emit(events));
        let sa_engine::adapter::AdapterEvent::Journal { kind, data } = rx.try_recv().unwrap()
        else {
            panic!("missing timing journal")
        };
        assert_eq!(kind, "turn.timing");
        assert_eq!(data["cache"], "hit");
        assert!(data["discover_ms"].as_u64().unwrap() >= 25);
    }
}
