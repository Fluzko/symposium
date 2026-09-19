//! Private state belonging to plugin-hook aggregate rows.

use std::fmt;

use super::HookSessionCountTracker;
use crate::telemetry::schema::{EventId, PluginHookMetricsKey};

/// Private state paired with one plugin-hook aggregate row.
///
/// The stable event identifier and session tracker live together so a row
/// update cannot select them independently. The eventual persisted aggregate
/// map will own these entries and decide when a new one is admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::telemetry) struct PluginHookAggregateState {
    event_id: EventId,
    session_counts: HookSessionCountTracker<PluginHookMetricsKey>,
}

impl PluginHookAggregateState {
    /// Start private state for a newly admitted aggregate key.
    ///
    /// Kept inside the state module so producers cannot bypass the eventual
    /// daily public-row admission policy.
    #[must_use]
    pub(super) fn new(key: PluginHookMetricsKey) -> Self {
        Self {
            event_id: EventId::new(),
            session_counts: HookSessionCountTracker::new(key),
        }
    }

    /// Start an entry without bypassing production admission outside tests.
    #[cfg(test)]
    #[must_use]
    pub(in crate::telemetry) fn for_test(key: PluginHookMetricsKey) -> Self {
        Self::new(key)
    }

    /// Select the event identifier and session tracker for `key` together.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHookAggregateSelectionError`] when this entry belongs
    /// to another aggregate key.
    pub(in crate::telemetry) fn select(
        &mut self,
        key: &PluginHookMetricsKey,
    ) -> Result<SelectedPluginHookAggregate<'_>, PluginHookAggregateSelectionError> {
        if self.session_counts.key() != key {
            return Err(PluginHookAggregateSelectionError);
        }

        Ok(SelectedPluginHookAggregate {
            event_id: self.event_id,
            session_counts: &mut self.session_counts,
        })
    }
}

/// One key-checked view of a plugin-hook aggregate's private state.
pub(in crate::telemetry) struct SelectedPluginHookAggregate<'a> {
    event_id: EventId,
    session_counts: &'a mut HookSessionCountTracker<PluginHookMetricsKey>,
}

impl SelectedPluginHookAggregate<'_> {
    /// Return the stable identifier of the selected aggregate row.
    #[must_use]
    pub(in crate::telemetry) const fn event_id(&self) -> EventId {
        self.event_id
    }

    /// Return the aggregate key selected with the row identifier.
    #[must_use]
    pub(in crate::telemetry) const fn key(&self) -> &PluginHookMetricsKey {
        self.session_counts.key()
    }

    /// Borrow the session tracker selected with the row identifier.
    #[must_use]
    pub(in crate::telemetry) fn session_counts(
        &mut self,
    ) -> &mut HookSessionCountTracker<PluginHookMetricsKey> {
        self.session_counts
    }
}

/// A private-state entry selected with another plugin-hook aggregate key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct PluginHookAggregateSelectionError;

impl fmt::Display for PluginHookAggregateSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("plugin-hook private state belongs to another aggregate")
    }
}

impl std::error::Error for PluginHookAggregateSelectionError {}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::telemetry::{
        identity::SessionId,
        schema::{HookAgent, HookSurface, PluginBucket, PluginHookOutcome, UtcDay},
        state::{IDENTIFIER_WINDOW_TEST_STATE, TelemetryStateV1, recording_observation},
    };

    fn state() -> TelemetryStateV1 {
        toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap()
    }

    fn key(state: &mut TelemetryStateV1, hook: HookSurface) -> PluginHookMetricsKey {
        let recording = recording_observation(state);

        PluginHookMetricsKey::new(&recording, HookAgent::Claude, hook, PluginBucket::Unnamed)
    }

    #[test]
    fn selection_keeps_one_event_id_and_tracker_with_their_key() {
        let mut state = state();
        let key = key(&mut state, HookSurface::PreToolUse);
        let mut aggregate = PluginHookAggregateState::new(key.clone());

        let first_event_id = {
            let mut selected = aggregate.select(&key).unwrap();
            let event_id = selected.event_id();
            assert_eq!(selected.session_counts().key(), &key);
            event_id
        };
        let second_event_id = aggregate.select(&key).unwrap().event_id();

        assert_eq!(second_event_id, first_event_id);
    }

    #[test]
    fn selection_rejects_another_aggregate_key() {
        let mut state = state();
        let selected_key = key(&mut state, HookSurface::PreToolUse);
        let other_key = key(&mut state, HookSurface::PostToolUse);
        let mut aggregate = PluginHookAggregateState::new(selected_key);

        let Err(error) = aggregate.select(&other_key) else {
            panic!("accepted private state belonging to another aggregate");
        };

        assert_eq!(error, PluginHookAggregateSelectionError);
    }

    #[test]
    fn selection_rejects_the_same_bucket_after_an_identifier_reset() {
        let mut state = state();
        let first_key = key(&mut state, HookSurface::PreToolUse);
        let mut aggregate = PluginHookAggregateState::new(first_key);
        state
            .reset_identifiers(UtcDay::from_date(
                NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
            ))
            .unwrap();
        let reset_key = key(&mut state, HookSurface::PreToolUse);

        let Err(error) = aggregate.select(&reset_key) else {
            panic!("accepted private state from the previous identifier epoch");
        };

        assert_eq!(error, PluginHookAggregateSelectionError);
    }

    #[test]
    fn debug_output_omits_tracked_session_identifiers() {
        let mut state = state();
        let key = key(&mut state, HookSurface::PreToolUse);
        let mut aggregate = PluginHookAggregateState::new(key.clone());
        let session_id: SessionId = "sess_00000000000000000000000000000001".parse().unwrap();
        aggregate
            .select(&key)
            .unwrap()
            .session_counts()
            .checked_record(0, Some(session_id), PluginHookOutcome::Ok)
            .unwrap();

        let debug = format!("{aggregate:?}");

        assert!(!debug.contains("sess_00000000000000000000000000000001"));
        assert!(debug.contains("identified_sessions: 1"));
    }
}
