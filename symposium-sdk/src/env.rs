//! Environment-variable helpers that emit predicate cache events.
//!
//! Use [`var`] instead of [`std::env::var`] from a custom predicate so that
//! reading a variable also declares a dependency on its value. Symposium
//! fingerprints the value at read time and invalidates the cached predicate
//! result if the value changes.

use std::env;

use crate::predicate::PredicateEmitter;

/// Read the value of the environment variable `name` and emit a
/// [`CustomPredicateEvent::WatchEnv`] on stdout so Symposium invalidates the
/// cached predicate result when the value changes.
///
/// [`CustomPredicateEvent::WatchEnv`]: crate::predicate::CustomPredicateEvent::WatchEnv
pub fn var(name: &str) -> Result<String, env::VarError> {
    let _ = PredicateEmitter::stdout().watch_env(name);
    env::var(name)
}
