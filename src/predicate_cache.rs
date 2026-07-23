//! Watch sets and fingerprints for custom predicate caching.
//!
//! A predicate emits [`CustomPredicateEvent`]s naming inputs whose changes
//! invalidate its result. Those events are unioned into a [`WatchSet`]. When
//! the predicate runs, we take a [`Fingerprints`] snapshot of the watched
//! inputs; on the next sync we compare a fresh snapshot to the stored one and
//! reuse the cached result while they match.
//!
//! This module is data-model only. It does not spawn processes, read the cache
//! file, or wire results into evaluation. Those live in follow-up commits.

#![allow(dead_code)] // wired into cache read/write in the next commit

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use symposium_sdk::predicate::CustomPredicateEvent;

/// The union of watched inputs and cache lifetime derived from one predicate
/// execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchSet {
    pub files: BTreeSet<PathBuf>,
    pub env: BTreeSet<String>,
    pub cache_ttl: CacheTtl,
}

/// How long the predicate result may be cached, independent of file / env
/// invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    /// No `WatchTime` events were emitted; the result never becomes stale by
    /// time alone.
    Forever,
    /// The shortest `WatchTime(N>0)` reported by the predicate.
    For(Duration),
    /// `WatchTime(0)` was emitted; the result must not be cached.
    Never,
}

impl Default for CacheTtl {
    fn default() -> Self {
        Self::Forever
    }
}

impl WatchSet {
    /// Union every event from a single predicate execution into one set.
    pub fn from_events(events: &[CustomPredicateEvent]) -> Self {
        let mut set = Self::default();
        for event in events {
            match event {
                CustomPredicateEvent::WatchFile(path) => {
                    set.files.insert(path.clone());
                }
                CustomPredicateEvent::WatchEnv(name) => {
                    set.env.insert(name.clone());
                }
                CustomPredicateEvent::WatchTime(0) => {
                    set.cache_ttl = CacheTtl::Never;
                }
                CustomPredicateEvent::WatchTime(ms) => {
                    let next = Duration::from_millis(*ms);
                    set.cache_ttl = match set.cache_ttl {
                        CacheTtl::Never => CacheTtl::Never,
                        CacheTtl::Forever => CacheTtl::For(next),
                        CacheTtl::For(current) => CacheTtl::For(current.min(next)),
                    };
                }
                _ => {}
            }
        }
        set
    }
}

/// Snapshot of the watched inputs at a point in time. Two snapshots that
/// compare equal mean nothing observable to the cache has changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprints {
    pub files: BTreeMap<PathBuf, FileFingerprint>,
    pub env: BTreeMap<String, Option<String>>,
}

/// A file's `mtime` in nanoseconds and byte size. Both are `None` when the
/// file is missing so an absent → present transition invalidates the entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileFingerprint {
    pub mtime_ns: Option<i128>,
    pub size: Option<u64>,
}

impl FileFingerprint {
    /// Fingerprint a file on disk. Missing files, unreadable metadata, and
    /// metadata without a usable timestamp all resolve to a deterministic
    /// `None`-only state; predicates fail to their conservative branch when
    /// the file transitions to a readable state.
    pub fn of(path: &Path) -> Self {
        match fs::metadata(path) {
            Ok(meta) => Self {
                size: Some(meta.len()),
                mtime_ns: meta
                    .modified()
                    .ok()
                    .and_then(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_nanos() as i128)
                            .or_else(|| {
                                std::time::UNIX_EPOCH
                                    .duration_since(t)
                                    .ok()
                                    .map(|d| -(d.as_nanos() as i128))
                            })
                    }),
            },
            Err(_) => Self::default(),
        }
    }
}

impl Fingerprints {
    /// Capture fingerprints for every input in `set`. Non-existent files and
    /// missing env vars are stored as their "absent" fingerprint so future
    /// appearances count as an invalidating change.
    pub fn capture(set: &WatchSet) -> Self {
        let files = set
            .files
            .iter()
            .map(|path| (path.clone(), FileFingerprint::of(path)))
            .collect();
        let env = set
            .env
            .iter()
            .map(|name| (name.clone(), std::env::var(name).ok()))
            .collect();
        Self { files, env }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_time_zero_produces_never_ttl() {
        let events = vec![
            CustomPredicateEvent::WatchTime(60_000),
            CustomPredicateEvent::WatchTime(0),
        ];
        let set = WatchSet::from_events(&events);
        assert_eq!(set.cache_ttl, CacheTtl::Never);
    }

    #[test]
    fn fingerprint_changes_when_file_grows() {
        use std::io::Write;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp.as_file(), "one").unwrap();

        let set = WatchSet {
            files: [tmp.path().to_path_buf()].into_iter().collect(),
            ..WatchSet::default()
        };
        let before = Fingerprints::capture(&set);

        // Wait long enough for the mtime to move on filesystems with 1s
        // resolution and grow the file so `size` shifts even when mtime does
        // not advance.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        writeln!(tmp.as_file(), "two").unwrap();
        tmp.as_file().sync_all().unwrap();

        let after = Fingerprints::capture(&set);
        assert_ne!(before, after);
    }
}
