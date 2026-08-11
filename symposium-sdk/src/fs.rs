//! Filesystem helpers that emit predicate cache events.
//!
//! Use these wrappers instead of [`std::fs`] from a custom predicate so that
//! reading a file also declares a dependency on its contents. Symposium
//! fingerprints the file (`mtime + size`) at read time and invalidates the
//! cached predicate result when the fingerprint changes.

use std::io;
use std::path::Path;

use crate::predicate::PredicateEmitter;

/// Read `path` to a string and emit a
/// [`CustomPredicateEvent::WatchFile`] on stdout so Symposium invalidates the
/// cached predicate result when the file changes.
///
/// [`CustomPredicateEvent::WatchFile`]: crate::predicate::CustomPredicateEvent::WatchFile
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    let _ = PredicateEmitter::stdout().watch_file(path.to_path_buf());
    std::fs::read_to_string(path)
}
