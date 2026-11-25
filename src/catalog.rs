//! The tools the account's catalog defines beyond this server's own.
//!
//! The definitions come from the API, through the engine, and are listed exactly as the API
//! describes them. They are read in the background and kept in memory and on disk, so
//! `tools/list` answers at once: with what was last known, and a change is announced when
//! the fresh read differs from what was listed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::engine::Engine;

/// The tools this server defines itself. A catalog tool never shadows one.
pub const BUILTIN: [&str; 5] = ["check", "check_parallel", "status", "setup", "run_output"];

/// How long a read of the catalog may take in the background.
const FETCH_BUDGET: Duration = Duration::from_secs(10);
/// How long what was read is used before it is read again.
const FRESH_FOR: Duration = Duration::from_secs(300);
/// How long a file on disk is trusted.
const DISK_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How long `tools/list` waits for a first read that is already under way.
pub const FIRST_READ_WAIT: Duration = Duration::from_millis(400);

pub fn is_builtin(name: &str) -> bool {
    BUILTIN.contains(&name)
}

#[derive(Default)]
struct Known {
    entries: Option<Vec<Value>>,
    read_at: Option<Instant>,
    /// What `tools/list` last showed for this account.
    listed: Option<String>,
    /// Set while a read is under way; resolves when it ends.
    reading: Option<watch::Receiver<bool>>,
}

/// What the server knows of each account's catalog.
#[derive(Default)]
pub struct Catalog {
    known: Mutex<HashMap<String, Known>>,
}

/// Called when a fresh read differs from what was listed.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

fn key_of(engine: &Engine) -> String {
    format!("{}\0{}", engine.fingerprint, engine.api_url)
}

fn disk_path(engine: &Engine) -> PathBuf {
    crate::config::cache_dir().join(format!("catalog-{}.json", engine.fingerprint))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn read_disk(engine: &Engine) -> Option<Vec<Value>> {
    let path = disk_path(engine);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > 4 << 20 {
        return None;
    }
    let saved: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let fresh = saved
        .get("saved_at")
        .and_then(Value::as_u64)
        .is_some_and(|at| now_secs().saturating_sub(at) < DISK_MAX_AGE.as_secs());
    if !fresh || saved.get("api_url").and_then(Value::as_str) != Some(engine.api_url.as_str()) {
        return None;
    }
    saved.get("tools").and_then(Value::as_array).cloned()
}

fn write_disk(engine: &Engine, entries: &[Value]) {
    let path = disk_path(engine);
    let Some(parent) = path.parent() else {
        return;
    };
    let body = json!({"api_url": engine.api_url, "saved_at": now_secs(), "tools": entries});
    let written = std::fs::create_dir_all(parent).and_then(|()| {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        crate::credentials::set_private(temporary.path())?;
        std::io::Write::write_all(&mut temporary, body.to_string().as_bytes())?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    });
    if let Err(error) = written {
        tracing::debug!(%error, "the catalog was not cached on disk");
    }
}

impl Catalog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The catalog's entries for a listing, at once: what is known in memory, else what the
    /// disk holds, else nothing yet. A read is started when there is none or it is old; a
    /// first read already under way is waited for briefly.
    pub async fn for_listing(
        self: &Arc<Self>,
        engine: &Arc<Engine>,
        on_change: OnChange,
    ) -> Vec<Value> {
        let key = key_of(engine);
        let waiting = self.refresh_if_stale(engine, on_change);
        let unknown = self
            .known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_none_or(|known| known.entries.is_none());
        if let (true, Some(mut reading)) = (unknown, waiting) {
            let _ = tokio::time::timeout(FIRST_READ_WAIT, async {
                while !*reading.borrow() {
                    if reading.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let known = known.entry(key).or_default();
        let entries = match &known.entries {
            Some(entries) => entries.clone(),
            None => read_disk(engine).unwrap_or_default(),
        };
        known.listed = Some(Value::Array(entries.clone()).to_string());
        entries
    }

    /// Start a background read unless a fresh one is known or one is under way.
    pub fn refresh_if_stale(
        self: &Arc<Self>,
        engine: &Arc<Engine>,
        on_change: OnChange,
    ) -> Option<watch::Receiver<bool>> {
        let key = key_of(engine);
        let mut all = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let known = all.entry(key.clone()).or_default();
        if let Some(reading) = &known.reading {
            return Some(reading.clone());
        }
        if known.read_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
            return None;
        }
        let (done, reading) = watch::channel(false);
        known.reading = Some(reading.clone());
        drop(all);
        let catalog = self.clone();
        let engine = engine.clone();
        tokio::spawn(async move {
            let entries = engine.catalog_entries(FETCH_BUDGET, is_builtin).await;
            let changed = catalog.record(&engine, &key, entries);
            let _ = done.send(true);
            if changed {
                on_change();
            }
        });
        Some(reading)
    }

    /// Read the catalog again now (after a sign-in or a changed setting) and return what a
    /// listing would show.
    pub async fn read_now(self: &Arc<Self>, engine: &Arc<Engine>) -> Vec<Value> {
        let entries = engine.catalog_entries(FETCH_BUDGET, is_builtin).await;
        self.record(engine, &key_of(engine), entries.clone());
        entries
    }

    /// Keep one read. True when it differs from what was last listed.
    fn record(&self, engine: &Engine, key: &str, entries: Vec<Value>) -> bool {
        write_disk(engine, &entries);
        let mut all = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let known = all.entry(key.to_string()).or_default();
        let now = Value::Array(entries.clone()).to_string();
        let changed = known.listed.as_ref().is_some_and(|listed| *listed != now);
        known.entries = Some(entries);
        known.read_at = Some(Instant::now());
        known.reading = None;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_own_tools_are_never_shadowed() {
        for name in BUILTIN {
            assert!(is_builtin(name));
        }
        assert!(!is_builtin("echo"));
        assert!(!is_builtin("verify"));
    }
}
