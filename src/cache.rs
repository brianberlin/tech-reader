//! PCM-by-text cache (ported behavior of the TS synth cache, §7.1.6).
//!
//! Synthesis is the expensive stage, and the same sentence often recurs
//! (repeated headings, boilerplate, and — in M5 — seek-back to an
//! already-spoken line). We cache the **raw, voice-native** mono PCM keyed by
//! the normalized sentence text so a repeat is an `Arc` clone instead of a
//! re-synthesis.
//!
//! Two design constraints from the spec:
//!  - **Capped by total bytes, not entry count** (N7): a long document must not
//!    grow the cache without bound. Eviction is LRU so the most recently spoken
//!    (and most likely sought-back) sentences survive.
//!  - **Single active speed** (§6.4): entries are valid only for the current
//!    `length_scale`; a speed change clears the cache (wired in M5). Within a run
//!    the model and scale are fixed, so the in-memory key is just the text.
//!
//! The cache lives on (and is only touched by) the synth worker thread, so it
//! needs no locking.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Collapse all whitespace runs to single spaces and trim — the cache key.
/// Streamed sentences are already squeezed; this keeps hits stable regardless.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

/// A byte-bounded LRU map from normalized sentence text to its synthesized PCM.
pub struct PcmCache {
    map: HashMap<String, Arc<[f32]>>,
    /// Recency queue: front = least-recently-used, back = most-recent.
    order: VecDeque<String>,
    bytes: usize,
    cap_bytes: usize,
}

impl PcmCache {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap_bytes,
        }
    }

    /// Total bytes of PCM currently held.
    pub fn len_bytes(&self) -> usize {
        self.bytes
    }

    /// Number of cached sentences.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Drop everything. Used on a speed change — cached PCM is valid only for the
    /// `length_scale` it was synthesized at, so a new speed starts fresh (§6.4).
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// Look up `key`; on a hit, mark it most-recently-used and return a cheap
    /// `Arc` clone of its samples.
    pub fn get(&mut self, key: &str) -> Option<Arc<[f32]>> {
        if !self.map.contains_key(key) {
            return None;
        }
        self.touch(key);
        self.map.get(key).cloned()
    }

    /// Insert (or refresh) `key`'s samples, then evict LRU entries until the
    /// total is back under the byte cap. An entry larger than the whole cap is
    /// admitted and then immediately evicted (it simply won't benefit a repeat).
    pub fn insert(&mut self, key: String, samples: Arc<[f32]>) {
        let add = samples.len() * BYTES_PER_SAMPLE;
        if let Some(old) = self.map.insert(key.clone(), samples) {
            self.bytes -= old.len() * BYTES_PER_SAMPLE;
            self.remove_from_order(&key);
        }
        self.bytes += add;
        self.order.push_back(key);
        self.evict_to_cap();
    }

    fn touch(&mut self, key: &str) {
        self.remove_from_order(key);
        self.order.push_back(key.to_string());
    }

    fn remove_from_order(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
    }

    fn evict_to_cap(&mut self) {
        while self.bytes > self.cap_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&oldest) {
                self.bytes -= v.len() * BYTES_PER_SAMPLE;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(n: usize) -> Arc<[f32]> {
        Arc::from(vec![0.0f32; n])
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  hello   world \n"), "hello world");
        assert_eq!(normalize("a\tb"), "a b");
    }

    #[test]
    fn hit_returns_same_allocation() {
        let mut c = PcmCache::new(1 << 20);
        let a = pcm(10);
        c.insert(normalize("one"), Arc::clone(&a));
        let got = c.get("one").expect("hit");
        assert!(Arc::ptr_eq(&a, &got), "cache should hand back the same Arc");
        assert!(c.get("missing").is_none());
    }

    #[test]
    fn byte_accounting() {
        let mut c = PcmCache::new(1 << 20);
        c.insert("a".into(), pcm(4)); // 16 bytes
        c.insert("b".into(), pcm(6)); // 24 bytes
        assert_eq!(c.len(), 2);
        assert_eq!(c.len_bytes(), (4 + 6) * 4);
    }

    #[test]
    fn evicts_oldest_when_over_cap() {
        // Cap holds two 4-sample entries (32 bytes) but not three.
        let mut c = PcmCache::new(32);
        c.insert("a".into(), pcm(4));
        c.insert("b".into(), pcm(4));
        c.insert("c".into(), pcm(4)); // pushes out "a"
        assert!(c.get("a").is_none(), "oldest should be evicted");
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
        assert_eq!(c.len_bytes(), 32);
    }

    #[test]
    fn lru_touch_protects_recent() {
        let mut c = PcmCache::new(32); // room for two
        c.insert("a".into(), pcm(4));
        c.insert("b".into(), pcm(4));
        // Touch "a" so it is most-recently-used; "b" becomes the eviction target.
        assert!(c.get("a").is_some());
        c.insert("c".into(), pcm(4));
        assert!(c.get("a").is_some(), "touched entry should survive");
        assert!(c.get("b").is_none(), "untouched entry should be evicted");
    }

    #[test]
    fn oversized_entry_self_evicts() {
        let mut c = PcmCache::new(16); // 4 samples max
        c.insert("big".into(), pcm(100)); // 400 bytes > cap
        assert_eq!(c.len(), 0, "an entry larger than the cap is not retained");
        assert_eq!(c.len_bytes(), 0);
    }
}
