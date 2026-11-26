//! Reusable results of model/capability probes. Credentials and launch
//! configuration stay with the adapter and are never serialized here.

use crate::{common::Emit, custom, sys::Sys};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
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
    fn invalidate_through(&mut self, cutoff: Stamp) {
        if !self.started.newer_than(cutoff) && self.value.take().is_some() {
            self.started = cutoff;
        }
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
    file: Option<PathBuf>,
    sources: HashMap<ProbeKey, Slots>,
    freshness: Duration,
}

impl State {
    fn open(file: Option<PathBuf>) -> Self {
        let now = Stamp::now();
        let sources = file
            .as_ref()
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
            file,
            sources,
            freshness: FRESHNESS,
        }
    }

    fn prune_and_write(&mut self) {
        let freshness = self.freshness;
        self.sources.retain(|_, slots| {
            slots.retain(|_, item| item.started.age() < freshness);
            !slots.is_empty()
        });
        // Cache I/O is best effort. Keep the in-memory result on any I/O error.
        let _ = self.persist();
    }

    fn persist(&self) -> anyhow::Result<()> {
        let Some(path) = &self.file else {
            return Ok(());
        };
        let dir = path.parent().expect("cache file has a parent");
        std::fs::create_dir_all(dir)?;
        let entries: Vec<_> = self.sources.iter().flat_map(|(key, slots)| {
            slots.iter().map(move |(slot, entry)| json!({
                "key": key, "slot": slot, "started_unix_ms": entry.started.unix_ms, "value": entry.value,
            }))
        }).collect();
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(
            temp.as_file_mut(),
            &json!({"version": VERSION, "entries": entries}),
        )?;
        temp.persist(path)?;
        Ok(())
    }
}

/// Instances targeting one data directory share a lock so different adapters
/// can update the same file without overwriting each other's entries.
#[derive(Clone)]
pub(crate) struct ProbeCache(Arc<Mutex<State>>);

type OpenCaches = HashMap<PathBuf, Weak<Mutex<State>>>;
static OPEN: LazyLock<Mutex<OpenCaches>> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl ProbeCache {
    pub(crate) fn new(data_dir: Option<&Path>) -> Self {
        let Some(dir) = data_dir else {
            return Self(Arc::new(Mutex::new(State::open(None))));
        };
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        let file = dir.join("cache/probes-v1.json");
        let mut open = OPEN.lock().unwrap();
        open.retain(|_, instance| instance.strong_count() > 0);
        if let Some(state) = open.get(&file).and_then(Weak::upgrade) {
            return Self(state);
        }
        let state = Arc::new(Mutex::new(State::open(Some(file.clone()))));
        open.insert(file, Arc::downgrade(&state));
        Self(state)
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, key: &ProbeKey, slot: &str) -> Option<T> {
        let state = self.0.lock().unwrap();
        let item = state.sources.get(key)?.get(slot)?;
        if item.started.age() >= state.freshness {
            return None;
        }
        serde_json::from_value(item.value.clone()?).ok()
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
        let mut state = self.0.lock().unwrap();
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
        let value = value.and_then(|v| serde_json::to_value(v).ok());
        slots.insert(slot.into(), Fetched { value, started });
        state.prune_and_write();
    }

    pub(crate) fn evict_through(&self, key: &ProbeKey, cutoff: Stamp) {
        let mut state = self.0.lock().unwrap();
        if let Some(slots) = state.sources.get_mut(key) {
            for entry in slots.values_mut() {
                entry.invalidate_through(cutoff);
            }
        }
        state.prune_and_write();
    }

    #[cfg(test)]
    pub(crate) fn set_freshness(&self, freshness: Duration) {
        self.0.lock().unwrap().freshness = freshness;
    }
}

/// Track which work a single discovery attempt actually performed.
pub(crate) struct Probes<'a> {
    pub(crate) cache: &'a ProbeCache,
    pub(crate) key: &'a ProbeKey,
    reuse: bool,
    hit: AtomicBool,
    probed: AtomicBool,
}

impl<'a> Probes<'a> {
    pub(crate) fn new(cache: &'a ProbeCache, key: &'a ProbeKey, reuse: bool) -> Self {
        Self {
            cache,
            key,
            reuse,
            hit: false.into(),
            probed: false.into(),
        }
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, slot: &str) -> Option<T> {
        let found = self
            .reuse
            .then(|| self.cache.read(self.key, slot))
            .flatten();
        if found.is_some() {
            self.hit.store(true, Relaxed);
        }
        found
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
        probe: impl Future<Output = anyhow::Result<Vec<Value>>>,
    ) -> anyhow::Result<Vec<Value>> {
        if let Some(models) = self.read(slot) {
            return Ok(models);
        }
        let started = self.start();
        let result = probe.await;
        if !cancel.is_cancelled() {
            let models = result
                .as_ref()
                .ok()
                .filter(|models| !models.is_empty())
                .map(|models| safe_models(&json!(models)));
            self.cache.record(self.key, slot, started, models.as_ref());
        }
        result
    }
}

pub(crate) fn probe_slot(kind: &str, identity: &Value) -> String {
    format!("{kind}:{}", digest(identity))
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
        let slot = probe_slot("custom", &json!(id));
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
                probes.cache.record(
                    probes.key,
                    &probe_slot("custom", &json!(id)),
                    started,
                    models.as_ref(),
                );
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

/// Retain the model data used by routing and effort selection. Discovery
/// diagnostics, provider labels and arbitrary nested metadata stay out of disk.
pub(crate) fn safe_models(models: &Value) -> Vec<Value> {
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row["id"].as_str()?;
            if id != "@native" && id.contains('@') {
                return None;
            }
            let mut result = json!({"id": id, "label": row["label"].as_str()
            .filter(|label| !label.contains('@')).unwrap_or(id)});
            for name in ["resolvedModel", "defaultReasoningEffort"] {
                if let Some(value) = row[name].as_str().filter(|value| !value.contains('@')) {
                    result[name] = json!(value);
                }
            }
            if let Some(levels) = row["reasoningEfforts"].as_array() {
                result["reasoningEfforts"] = json!(
                    levels
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|level| !level.contains('@'))
                        .collect::<Vec<_>>()
                );
            }
            if let Some(mapping) = row["reasoningEffortMapping"].as_object() {
                result["reasoningEffortMapping"] = Value::Object(
                    mapping
                        .iter()
                        .filter(|(key, value)| {
                            !key.contains('@')
                                && value.as_str().is_some_and(|value| !value.contains('@'))
                        })
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
            ms: if hit && !refreshed {
                0
            } else {
                started.elapsed().as_millis()
            },
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
    if cache == "hit" {
        assert_eq!(timing["discover_ms"], 0);
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
        assert_eq!(cache.0.lock().unwrap().sources.len(), 2);
        cache.set_freshness(Duration::from_secs(1));
        cache.record::<String>(&current, "models", now, None);
        assert_eq!(cache.0.lock().unwrap().sources.len(), 1);
        assert!(cache.0.lock().unwrap().sources.contains_key(&current));
    }

    #[test]
    fn persisted_entries_ignore_unknown_adapters_and_future_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let key = key("source");
        {
            let cache = ProbeCache::new(Some(dir.path()));
            cache.record(&key, "models", Stamp::now(), Some(&"valid"));
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
        let disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["entries"].as_array().unwrap().len(), 1);
    }
}
