//! Values computed off the UI thread and kept for a while, so slow work
//! (running `git`) never stalls drawing: a read returns what is known and,
//! when it is stale, starts a refresh in the background. Values are shared,
//! not copied, and dropped once nothing has asked for them for a while.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Entries not read for this long are dropped.
const EVICT_AFTER: Duration = Duration::from_mins(10);

#[derive(Debug)]
struct Entry<V> {
    value: Option<Arc<V>>,
    /// What the value was computed for, besides its key.
    made_for: Vec<String>,
    at: Instant,
    /// When it was last read.
    used: Instant,
    running: bool,
}

/// Values by folder, refreshed in the background after `ttl`.
#[derive(Debug)]
pub struct Refresher<V> {
    ttl: Duration,
    entries: Arc<Mutex<HashMap<PathBuf, Entry<V>>>>,
}

impl<V: Send + Sync + 'static> Refresher<V> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The latest value for `key` (`None` until a first one is ready). If
    /// it is older than the TTL, or was made for other inputs than
    /// `made_for`, `compute` runs on a thread to replace it.
    pub fn get(
        &self,
        key: &Path,
        made_for: Vec<String>,
        compute: impl FnOnce() -> Option<V> + Send + 'static,
    ) -> Option<Arc<V>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, e| e.running || e.used.elapsed() < EVICT_AFTER);
        let entry = entries.entry(key.to_path_buf()).or_insert_with(|| Entry {
            value: None,
            made_for: made_for.clone(),
            at: Instant::now(),
            used: Instant::now(),
            running: false,
        });
        entry.used = Instant::now();
        let fresh = entry.value.is_some() && entry.at.elapsed() < self.ttl;
        if (!fresh || entry.made_for != made_for) && !entry.running {
            entry.running = true;
            let entries = Arc::clone(&self.entries);
            let key = key.to_path_buf();
            std::thread::spawn(move || {
                let value = compute();
                let mut entries = entries.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(entry) = entries.get_mut(&key) {
                    entry.value = value.map(Arc::new);
                    entry.made_for = made_for;
                    entry.at = Instant::now();
                    entry.running = false;
                }
            });
        }
        entry.value.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_in_the_background_then_serves_the_value() {
        let cache: Refresher<u32> = Refresher::new(Duration::from_secs(60));
        let key = Path::new("/repo");
        assert_eq!(
            cache.get(key, Vec::new(), || Some(7)),
            None,
            "not ready yet"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.get(key, Vec::new(), || Some(8)).is_none() {
            assert!(Instant::now() < deadline, "the value never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            cache.get(key, Vec::new(), || Some(9)).as_deref(),
            Some(&7),
            "still fresh"
        );
    }
}
