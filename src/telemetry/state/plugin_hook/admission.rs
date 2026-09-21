//! Daily lookup for plugin-hook aggregates.

use std::{
    collections::{
        BTreeMap,
        btree_map::{Entry, OccupiedEntry, VacantEntry},
    },
    fmt,
};

use super::{
    AdmittedPluginBucket, PluginHookAggregateSelectionError, PluginHookAggregateState,
    PluginHookMetricsKey, SelectedPluginHookAggregate,
};
use crate::telemetry::{
    schema::{HookAgent, HookSurface, PluginHookAttribution, UtcDay},
    state::{
        BoundRecordingObservation, DayBeforeCurrent,
        open_day::{OpenDay, OpenDayUpdate},
    },
};

/// Daily private state for plugin-hook aggregate selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::telemetry) struct PluginHookAggregateStore {
    day: OpenDay,
    entries: BTreeMap<PluginHookMetricsKey, PluginHookAggregateState>,
}

impl PluginHookAggregateStore {
    #[must_use]
    pub(in crate::telemetry) const fn new(day: UtcDay) -> Self {
        Self {
            day: OpenDay::new(day),
            entries: BTreeMap::new(),
        }
    }

    /// Select or create the aggregate for one plugin-hook attempt.
    ///
    /// A forward UTC-day transition clears the previous entries. Public-row
    /// admission is added separately so this map can first establish one
    /// key-checked home for each aggregate.
    ///
    /// # Errors
    ///
    /// Returns an error if the observation day moves backward or stored
    /// private state does not match its map key.
    pub(in crate::telemetry) fn select(
        &mut self,
        recording: &BoundRecordingObservation<'_>,
        agent: HookAgent,
        surface: HookSurface,
        attribution: PluginHookAttribution,
    ) -> Result<SelectedPluginHookAggregate<'_>, PluginHookAdmissionError> {
        if self.day.select(recording.day())? == OpenDayUpdate::Advanced {
            self.entries.clear();
        }

        let bucket = AdmittedPluginBucket::from_attribution(recording, attribution);
        let key = PluginHookMetricsKey::new(recording, agent, surface, &bucket);
        match self.entries.entry(key) {
            Entry::Occupied(entry) => Self::select_occupied(entry),
            Entry::Vacant(entry) => Self::insert_vacant(entry, bucket),
        }
    }

    fn select_occupied(
        entry: OccupiedEntry<'_, PluginHookMetricsKey, PluginHookAggregateState>,
    ) -> Result<SelectedPluginHookAggregate<'_>, PluginHookAdmissionError> {
        let key = entry.key().clone();
        entry
            .into_mut()
            .select(&key)
            .map_err(PluginHookAdmissionError::PrivateState)
    }

    fn insert_vacant(
        entry: VacantEntry<'_, PluginHookMetricsKey, PluginHookAggregateState>,
        bucket: AdmittedPluginBucket,
    ) -> Result<SelectedPluginHookAggregate<'_>, PluginHookAdmissionError> {
        let state = PluginHookAggregateState::new(entry.key(), bucket);
        let key = entry.key().clone();
        entry
            .insert(state)
            .select(&key)
            .map_err(PluginHookAdmissionError::PrivateState)
    }

    /// Remove entries from the previous identifier epoch.
    pub(in crate::telemetry) fn reset_identifier_epoch(&mut self) {
        self.entries.clear();
    }

    /// Remove all current entries.
    pub(in crate::telemetry) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Failure while selecting private plugin-hook aggregate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum PluginHookAdmissionError {
    DayBeforeCurrent(DayBeforeCurrent),
    PrivateState(PluginHookAggregateSelectionError),
}

impl From<DayBeforeCurrent> for PluginHookAdmissionError {
    fn from(error: DayBeforeCurrent) -> Self {
        Self::DayBeforeCurrent(error)
    }
}

impl fmt::Display for PluginHookAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DayBeforeCurrent(error) => write!(formatter, "plugin-hook admission {error}"),
            Self::PrivateState(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PluginHookAdmissionError {}

#[cfg(test)]
mod tests;
