//! Session-local bounded decoded data cache. No geometry or result caching.
use crate::model::Raster;
use crate::source::WindowSource;
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};

pub const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Cumulative operation counters; bytes are payloads including validity.
#[derive(Clone, Copy, Default, Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub hit_payload_bytes: u64,
    pub admitted_payload_bytes: u64,
    pub admissions: u64,
    pub admission_rejections: u64,
    pub evictions: u64,
    pub evicted_accounted_bytes: u64,
    pub clears: u64,
}

/// Source interpretation is independent from its live transport locator.
/// Verification remains mandatory before lookup and after the query. Custom
/// adapters retain responsibility for their declared immutable source identity.
pub fn source_key(source: &dyn WindowSource) -> anyhow::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&serde_json::json!({
        "schema":"skarve_decoded_source_v1", "metadata": source.metadata(),
        "authority":source.identity_descriptor(), "mask":"adapter_normalized_independent_band_validity"
    }))?).to_hex().to_string())
}

pub fn window_key(source: &str, window: [usize; 4], bands: &[usize]) -> String {
    format!("{source}:{window:?}:{bands:?}")
}

#[derive(Default)]
pub struct TileCache {
    entries: HashMap<String, (Arc<Raster>, u64, usize)>,
    clock: u64,
    bytes: usize,
    limit: usize,
    stats: CacheStats,
}
impl TileCache {
    pub fn set_limit(&mut self, limit: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            limit <= MAX_CACHE_BYTES,
            "decoded cache exceeds 128 MiB session limit"
        );
        self.limit = limit;
        self.evict_to(limit);
        Ok(())
    }
    pub fn clear(&mut self) {
        self.entries = HashMap::new();
        self.bytes = 0;
        self.stats.clears += 1;
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn stats(&self) -> CacheStats {
        self.stats
    }
    /// Planning probe only: does not promote LRU order, clone a raster or alter stats.
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    pub fn get(&mut self, key: &str) -> Option<Arc<Raster>> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(key) {
            entry.1 = self.clock;
            self.stats.hits += 1;
            self.stats.hit_payload_bytes += payload_bytes(&entry.0);
            Some(Arc::clone(&entry.0))
        } else {
            self.stats.misses += 1;
            None
        }
    }
    pub fn insert(&mut self, key: String, raster: Arc<Raster>) {
        // Include conservative map/key/Arc bookkeeping in the advertised limit.
        let bytes = raster
            .bytes()
            .saturating_add(key.capacity())
            .saturating_add(256);
        if bytes > self.limit {
            self.stats.admission_rejections += 1;
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.2;
        }
        self.evict_to(self.limit - bytes);
        self.clock = self.clock.wrapping_add(1);
        self.stats.admissions += 1;
        self.stats.admitted_payload_bytes += payload_bytes(&raster);
        self.entries.insert(key, (raster, self.clock, bytes));
        self.bytes += bytes;
    }
    fn evict_to(&mut self, maximum: usize) {
        while self.bytes > maximum {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, value)| value.1)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            let removed = self
                .entries
                .remove(&key)
                .expect("selected cache key exists")
                .2;
            self.bytes -= removed;
            self.stats.evictions += 1;
            self.stats.evicted_accounted_bytes += removed as u64;
        }
        // Do not retain a formerly large bucket allocation after a budget shrink
        // or replacement by a small number of larger tiles. Per-entry accounting
        // assumes the bucket capacity remains proportional to the live entries.
        if self.entries.capacity() > self.entries.len().saturating_mul(2) {
            self.entries.shrink_to_fit();
        }
    }
}

pub fn payload_bytes(raster: &Raster) -> u64 {
    raster
        .bands
        .iter()
        .map(|b| (b.values.len() * 8 + b.valid.len()) as u64)
        .sum()
}
