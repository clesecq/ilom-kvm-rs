//! Last frame and resolution of recently used hosts, kept in memory so the
//! viewer can show something while a new session starts.
//!
//! Nothing here ever reaches the disk: a frame can show sensitive content.
//! Single-use JNLP data (`secret`, `username`) is never cached either.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use ilom_kvm_core::session::DecodedFrame;

/// Most hosts remembered at once.
pub const MAX_ENTRIES: usize = 8;
/// Most frame bytes remembered at once (RGBA, so `width * height * 4` each).
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

struct Entry {
    frame: Arc<DecodedFrame>,
    seen: Instant,
    /// Use counter: the smallest is the least recently used.
    order: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.frame.rgba.len()
    }
}

pub struct HostCache {
    entries: HashMap<String, Entry>,
    tick: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for HostCache {
    fn default() -> Self {
        Self::with_limits(MAX_ENTRIES, MAX_BYTES)
    }
}

impl HostCache {
    pub fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            max_entries,
            max_bytes,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of all remembered frames.
    pub fn bytes(&self) -> usize {
        self.entries.values().map(Entry::bytes).sum()
    }

    /// Remembers `frame` for `name`, replacing the previous one, and drops
    /// the least recently used hosts until the limits hold. A frame larger
    /// than the whole budget is not kept.
    pub fn insert(&mut self, name: &str, frame: Arc<DecodedFrame>) {
        self.tick += 1;
        self.entries.insert(
            name.to_owned(),
            Entry {
                frame,
                seen: Instant::now(),
                order: self.tick,
            },
        );
        while self.entries.len() > self.max_entries || self.bytes() > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.order)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    /// The remembered frame; counts as a use.
    pub fn frame(&mut self, name: &str) -> Option<Arc<DecodedFrame>> {
        self.tick += 1;
        let entry = self.entries.get_mut(name)?;
        entry.order = self.tick;
        Some(entry.frame.clone())
    }

    /// Frame size and age, for a tooltip. Not a use.
    pub fn describe(&self, name: &str) -> Option<((u32, u32), Duration)> {
        let entry = self.entries.get(name)?;
        Some((
            (entry.frame.width, entry.frame.height),
            entry.seen.elapsed(),
        ))
    }

    /// Forgets one host: its login was rejected, or its entry changed.
    pub fn clear_host(&mut self, name: &str) {
        self.entries.remove(name);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32) -> Arc<DecodedFrame> {
        Arc::new(DecodedFrame {
            width,
            height,
            sequence: 1,
            rgba: vec![0; (width * height * 4) as usize],
        })
    }

    #[test]
    fn insert_replaces_the_entry() {
        let mut cache = HostCache::default();
        cache.insert("a", frame(2, 2));
        let newer = frame(4, 4);
        cache.insert("a", newer.clone());
        assert_eq!(cache.len(), 1);
        assert!(Arc::ptr_eq(&cache.frame("a").unwrap(), &newer));
        assert_eq!(cache.bytes(), 4 * 4 * 4);
        assert_eq!(cache.describe("a").unwrap().0, (4, 4));
    }

    #[test]
    fn least_recently_used_goes_first() {
        let mut cache = HostCache::with_limits(2, usize::MAX);
        cache.insert("a", frame(1, 1));
        cache.insert("b", frame(1, 1));
        // Using "a" makes "b" the oldest.
        assert!(cache.frame("a").is_some());
        cache.insert("c", frame(1, 1));
        assert!(cache.frame("b").is_none());
        assert!(cache.frame("a").is_some());
        assert!(cache.frame("c").is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn byte_budget_evicts_oldest_first() {
        // Each 10x10 frame is 400 bytes.
        let mut cache = HostCache::with_limits(8, 1000);
        cache.insert("a", frame(10, 10));
        cache.insert("b", frame(10, 10));
        assert_eq!(cache.len(), 2);
        cache.insert("c", frame(10, 10));
        assert!(cache.describe("a").is_none());
        assert!(cache.describe("b").is_some() && cache.describe("c").is_some());
        assert!(cache.bytes() <= 1000);
    }

    #[test]
    fn a_frame_over_the_whole_budget_is_not_kept() {
        let mut cache = HostCache::with_limits(8, 1000);
        cache.insert("a", frame(10, 10));
        cache.insert("big", frame(100, 100));
        // The oldest go first, and then the oversized frame itself.
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn clearing_one_host_or_all() {
        let mut cache = HostCache::default();
        cache.insert("a", frame(1, 1));
        cache.insert("b", frame(1, 1));
        cache.clear_host("a");
        cache.clear_host("missing");
        assert!(cache.describe("a").is_none());
        assert!(cache.describe("b").is_some());
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn default_limits_hold_with_realistic_frames() {
        let mut cache = HostCache::default();
        // 1920x1080 is about 8 MB: only four fit in 32 MB.
        for index in 0..6 {
            cache.insert(&format!("h{index}"), frame(1920, 1080));
        }
        assert!(cache.bytes() <= MAX_BYTES);
        assert!(cache.len() <= 4);
        assert!(cache.describe("h5").is_some());
        assert!(cache.describe("h0").is_none());
    }
}
