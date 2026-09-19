//! Hook aggregate vocabulary, counters, rows, and identity dimensions.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    EventId, RowKind, SchemaVersion, SymposiumVersion, UtcDay,
    agent::{HookAgent, VendorSessionId, derive_session_id},
    macros::strict_versioned_row,
    metrics::{
        LatencyHistogram, LatencyHistogramError, SessionCountError, SessionCountInput,
        validate_session_counts,
    },
};
use crate::{
    hook_schema::HookEvent,
    telemetry::{
        identity::{DimensionWriter, HookDomain, HookSubject, IdentityDimension},
        state::{
            BoundRecordingObservation, HookSessionCountSnapshot, HookSessionCountTracker,
            HookSessionCountUpdateError,
        },
    },
};

/// Hook surface included in version 1 aggregate telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::telemetry) enum HookSurface {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    Stop,
}

impl HookSurface {
    /// Return the frozen version 1 wire label.
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::SessionStart => "session_start",
            Self::Stop => "stop",
        }
    }
}

/// A hook event that version 1 telemetry does not aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct UnsupportedHookEvent;

impl fmt::Display for UnsupportedHookEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("hook event is not supported by telemetry consent version 1")
    }
}

impl std::error::Error for UnsupportedHookEvent {}

impl TryFrom<HookEvent> for HookSurface {
    type Error = UnsupportedHookEvent;

    fn try_from(event: HookEvent) -> Result<Self, Self::Error> {
        match event {
            HookEvent::PreToolUse => Ok(Self::PreToolUse),
            HookEvent::PostToolUse => Ok(Self::PostToolUse),
            HookEvent::UserPromptSubmit => Ok(Self::UserPromptSubmit),
            HookEvent::SessionStart => Ok(Self::SessionStart),
            HookEvent::Stop => Ok(Self::Stop),
            // HookEvent is non-exhaustive. New SDK events need an explicit
            // consent-contract decision before telemetry records them.
            _ => Err(UnsupportedHookEvent),
        }
    }
}

/// Final outcome assigned to one completed top-level hook observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum HookOutcome {
    Ok,
    Blocked,
    PluginError,
    InternalError,
}

impl HookOutcome {
    /// Select the final outcome using the version 1 precedence rule.
    #[must_use]
    pub(in crate::telemetry) const fn from_signals(signals: HookOutcomeSignals) -> Self {
        if signals.internal_error {
            Self::InternalError
        } else if signals.blocked {
            Self::Blocked
        } else if signals.plugin_error {
            Self::PluginError
        } else {
            Self::Ok
        }
    }

    /// Return the counter name associated with this outcome.
    #[must_use]
    const fn counter_name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Blocked => "blocked",
            Self::PluginError => "plugin_error",
            Self::InternalError => "internal_error",
        }
    }
}

/// Signals used to select one final top-level hook outcome.
///
/// Named fields prevent the three precedence inputs from being transposed at
/// call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct HookOutcomeSignals {
    pub(in crate::telemetry) internal_error: bool,
    pub(in crate::telemetry) blocked: bool,
    pub(in crate::telemetry) plugin_error: bool,
}

/// Mutually exclusive outcomes of completed top-level hook observations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookOutcomeCounters {
    ok: u64,
    blocked: u64,
    plugin_error: u64,
    internal_error: u64,
}

impl HookOutcomeCounters {
    /// Increment exactly one outcome counter.
    ///
    /// # Errors
    ///
    /// Returns [`HookOutcomeCounterOverflow`] when the selected counter cannot
    /// be incremented. The counters are unchanged on failure.
    #[must_use = "counter overflow must drop the containing telemetry update"]
    fn checked_record(&mut self, outcome: HookOutcome) -> Result<(), HookOutcomeCounterOverflow> {
        let counter = match outcome {
            HookOutcome::Ok => &mut self.ok,
            HookOutcome::Blocked => &mut self.blocked,
            HookOutcome::PluginError => &mut self.plugin_error,
            HookOutcome::InternalError => &mut self.internal_error,
        };
        let next = counter
            .checked_add(1)
            .ok_or(HookOutcomeCounterOverflow { outcome })?;

        *counter = next;
        Ok(())
    }

    /// Return the sum of every outcome counter, or `None` on overflow.
    #[must_use]
    fn checked_total(&self) -> Option<u64> {
        [
            self.ok,
            self.blocked,
            self.plugin_error,
            self.internal_error,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
    }
}

/// A selected hook outcome counter that cannot be incremented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HookOutcomeCounterOverflow {
    outcome: HookOutcome,
}

impl fmt::Display for HookOutcomeCounterOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} hook outcome counter overflows u64",
            self.outcome.counter_name()
        )
    }
}

impl std::error::Error for HookOutcomeCounterOverflow {}

/// Typed inputs to one `hook_subject` derivation.
struct HookDimension {
    agent: HookAgent,
    hook: HookSurface,
}

impl HookDimension {
    #[must_use]
    const fn new(agent: HookAgent, hook: HookSurface) -> Self {
        Self { agent, hook }
    }
}

impl IdentityDimension for HookDimension {
    type Domain = HookDomain;

    /// Write agent and hook surface in version 1 contract order.
    fn write(&self, writer: &mut DimensionWriter<'_>) {
        writer.field(self.agent.as_str().as_bytes());
        writer.field(self.hook.as_str().as_bytes());
    }
}

/// One completed top-level hook observation added to an aggregate row.
///
/// Named fields keep the agent, surface, counters, duration, and optional
/// vendor session identifier together at the write boundary.
// Intentionally omit `Debug`: this value borrows a raw vendor session id.
#[derive(Clone, Copy)]
pub(in crate::telemetry) struct HookMetricObservation<'a> {
    pub(in crate::telemetry) agent: HookAgent,
    pub(in crate::telemetry) hook: HookSurface,
    pub(in crate::telemetry) outcome: HookOutcome,
    pub(in crate::telemetry) plugins_attempted: u64,
    pub(in crate::telemetry) plugins_completed: u64,
    pub(in crate::telemetry) duration: Duration,
    pub(in crate::telemetry) vendor_session_id: Option<&'a VendorSessionId>,
}

/// Stable lookup key shared by one hook aggregate row and its private state.
///
/// The subject binds the agent, hook surface, identity key, and identifier
/// window. The day keeps daily aggregates separate within one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::telemetry) struct HookMetricsKey {
    day: UtcDay,
    hook_subject: HookSubject,
}

impl HookMetricsKey {
    /// Select the aggregate for one bound recording, agent, and hook surface.
    #[must_use]
    pub(in crate::telemetry) fn new(
        recording: &BoundRecordingObservation<'_>,
        agent: HookAgent,
        hook: HookSurface,
    ) -> Self {
        let hook_subject = recording
            .identifier_window_scope()
            .derive(&HookDimension::new(agent, hook));

        Self {
            day: recording.day(),
            hook_subject,
        }
    }
}

strict_versioned_row! {
    /// Version 1 daily aggregate for one agent and hook surface.
    pub(in crate::telemetry) struct HookMetricsV1 {
        symposium: SymposiumVersion,
        agent: HookAgent,
        hook: HookSurface,
        invocations: u64,
        outcomes: HookOutcomeCounters,
        plugins_attempted: u64,
        plugins_completed: u64,
        duration_ms: LatencyHistogram,
        #[serde(skip_serializing_if = "Option::is_none")]
        identified_sessions: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        identified_sessions_non_ok: Option<u64>,
        session_counts_complete: bool,
        hook_subject: HookSubject,
    }

    kind: RowKind::HookMetrics,
    raw: RawHookMetricsV1,
    validate: validate_hook_metrics,
}

impl HookMetricsV1 {
    /// Start an aggregate row with its first completed hook observation.
    ///
    /// The row day, hook subject, and optional session identifier all come
    /// from the same bound recording context. A private tracker that survives
    /// without its snapshot should be supplied here; the contribution-count
    /// mismatch makes its published session counts incomplete.
    ///
    /// # Errors
    ///
    /// Returns [`HookMetricsUpdateError`] when any counter cannot represent
    /// the observation or the supplied context does not select this row.
    #[must_use = "a failed hook metric update must be dropped"]
    pub(in crate::telemetry) fn new(
        recording: &BoundRecordingObservation<'_>,
        observation: HookMetricObservation<'_>,
        session_counts: &mut HookSessionCountTracker<HookMetricsKey>,
    ) -> Result<Self, HookMetricsUpdateError> {
        let key = HookMetricsKey::new(recording, observation.agent, observation.hook);
        let mut row = Self {
            version: SchemaVersion::V1,
            kind: Self::KIND,
            event_id: EventId::new(),
            day: recording.day(),
            symposium: SymposiumVersion::current(),
            agent: observation.agent,
            hook: observation.hook,
            invocations: 0,
            outcomes: HookOutcomeCounters::default(),
            plugins_attempted: 0,
            plugins_completed: 0,
            duration_ms: LatencyHistogram::default(),
            identified_sessions: None,
            identified_sessions_non_ok: None,
            session_counts_complete: false,
            hook_subject: key.hook_subject,
        };

        row.checked_record(recording, observation, session_counts)?;
        Ok(row)
    }

    /// Add one completed hook observation without partially changing either
    /// the row or its private session tracker.
    ///
    /// All row counters are updated on a clone first. The tracker update is
    /// the final fallible operation and guarantees it remains unchanged on
    /// failure; applying its resulting snapshot and replacing the row are
    /// infallible. The current row invocation count is always supplied to the
    /// tracker, making state/snapshot reconciliation part of every update.
    ///
    /// # Errors
    ///
    /// Returns [`HookMetricsUpdateError`] when the recording selects another
    /// aggregate or any counter would overflow. Both inputs remain unchanged
    /// on failure.
    #[must_use = "a failed hook metric update must be dropped"]
    pub(in crate::telemetry) fn checked_record(
        &mut self,
        recording: &BoundRecordingObservation<'_>,
        observation: HookMetricObservation<'_>,
        session_counts: &mut HookSessionCountTracker<HookMetricsKey>,
    ) -> Result<(), HookMetricsUpdateError> {
        self.ensure_selected_by(recording, observation, session_counts)?;

        if observation.plugins_completed > observation.plugins_attempted {
            return Err(HookMetricsUpdateError::CompletedPluginsExceedAttempts {
                attempted: observation.plugins_attempted,
                completed: observation.plugins_completed,
            });
        }

        let mut next = self.clone();
        next.invocations = next
            .invocations
            .checked_add(1)
            .ok_or(HookMetricsUpdateError::InvocationCountOverflow)?;
        next.outcomes.checked_record(observation.outcome)?;
        next.plugins_attempted = next
            .plugins_attempted
            .checked_add(observation.plugins_attempted)
            .ok_or(HookMetricsUpdateError::PluginAttemptCountOverflow)?;
        next.plugins_completed = next
            .plugins_completed
            .checked_add(observation.plugins_completed)
            .ok_or(HookMetricsUpdateError::PluginCompletionCountOverflow)?;
        next.duration_ms.checked_record(observation.duration)?;

        let session_id = derive_session_id(
            recording.identifier_window_scope(),
            observation.agent,
            observation.vendor_session_id,
        );
        // `invocations + 1` succeeded above, so the tracker's matching
        // contribution increment cannot overflow here.
        session_counts.checked_record(self.invocations, session_id, observation.outcome)?;
        next.apply_session_counts(session_counts.snapshot());

        *self = next;
        Ok(())
    }

    fn ensure_selected_by(
        &self,
        recording: &BoundRecordingObservation<'_>,
        observation: HookMetricObservation<'_>,
        session_counts: &HookSessionCountTracker<HookMetricsKey>,
    ) -> Result<(), HookMetricsUpdateError> {
        if self.day != recording.day() {
            return Err(HookMetricsUpdateError::DayChanged {
                row_day: self.day,
                observation_day: recording.day(),
            });
        }
        if self.agent != observation.agent || self.hook != observation.hook {
            return Err(HookMetricsUpdateError::TargetChanged);
        }

        let selected_key = HookMetricsKey::new(recording, observation.agent, observation.hook);
        if self.key() != selected_key {
            return Err(HookMetricsUpdateError::IdentifierEpochChanged);
        }
        if session_counts.key() != &selected_key {
            return Err(HookMetricsUpdateError::SessionTrackerChanged);
        }

        Ok(())
    }

    /// Return the lookup key shared with this aggregate's private state.
    #[must_use]
    pub(in crate::telemetry) const fn key(&self) -> HookMetricsKey {
        HookMetricsKey {
            day: self.day,
            hook_subject: self.hook_subject,
        }
    }

    fn apply_session_counts(&mut self, snapshot: HookSessionCountSnapshot) {
        match snapshot {
            HookSessionCountSnapshot::Complete {
                identified_sessions,
                identified_sessions_non_ok,
            } => {
                self.identified_sessions = Some(identified_sessions);
                self.identified_sessions_non_ok = Some(identified_sessions_non_ok);
                self.session_counts_complete = true;
            }
            HookSessionCountSnapshot::Incomplete => {
                self.identified_sessions = None;
                self.identified_sessions_non_ok = None;
                self.session_counts_complete = false;
            }
        }
    }
}

/// A hook aggregate update that cannot be represented safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum HookMetricsUpdateError {
    DayChanged {
        row_day: UtcDay,
        observation_day: UtcDay,
    },
    TargetChanged,
    IdentifierEpochChanged,
    SessionTrackerChanged,
    InvocationCountOverflow,
    OutcomeCountOverflow {
        outcome: HookOutcome,
    },
    PluginAttemptCountOverflow,
    PluginCompletionCountOverflow,
    CompletedPluginsExceedAttempts {
        attempted: u64,
        completed: u64,
    },
    Duration(LatencyHistogramError),
    SessionCounts(HookSessionCountUpdateError),
}

impl From<HookOutcomeCounterOverflow> for HookMetricsUpdateError {
    fn from(error: HookOutcomeCounterOverflow) -> Self {
        Self::OutcomeCountOverflow {
            outcome: error.outcome,
        }
    }
}

impl From<LatencyHistogramError> for HookMetricsUpdateError {
    fn from(error: LatencyHistogramError) -> Self {
        Self::Duration(error)
    }
}

impl From<HookSessionCountUpdateError> for HookMetricsUpdateError {
    fn from(error: HookSessionCountUpdateError) -> Self {
        Self::SessionCounts(error)
    }
}

impl fmt::Display for HookMetricsUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DayChanged {
                row_day,
                observation_day,
            } => write!(
                formatter,
                "hook metrics row belongs to {row_day}, not observed day {observation_day}"
            ),
            Self::TargetChanged => {
                formatter.write_str("hook observation targets another agent or hook surface")
            }
            Self::IdentifierEpochChanged => {
                formatter.write_str("hook observation belongs to another identifier epoch")
            }
            Self::SessionTrackerChanged => {
                formatter.write_str("hook session tracker belongs to another aggregate")
            }
            Self::InvocationCountOverflow => {
                formatter.write_str("hook invocation count overflows u64")
            }
            Self::OutcomeCountOverflow { outcome } => write!(
                formatter,
                "{} hook outcome counter overflows u64",
                outcome.counter_name()
            ),
            Self::PluginAttemptCountOverflow => {
                formatter.write_str("attempted plugin hook count overflows u64")
            }
            Self::PluginCompletionCountOverflow => {
                formatter.write_str("completed plugin hook count overflows u64")
            }
            Self::CompletedPluginsExceedAttempts {
                attempted,
                completed,
            } => write!(
                formatter,
                "completed plugin hooks {completed} exceed {attempted} attempted plugin hooks"
            ),
            Self::Duration(error) => fmt::Display::fmt(error, formatter),
            Self::SessionCounts(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for HookMetricsUpdateError {}

fn validate_hook_metrics(raw: &RawHookMetricsV1) -> Result<(), HookMetricsError> {
    if raw.invocations == 0 {
        return Err(HookMetricsError::NoInvocations);
    }

    let outcome_total = raw
        .outcomes
        .checked_total()
        .ok_or(HookMetricsError::OutcomeTotalOverflow)?;
    if outcome_total != raw.invocations {
        return Err(HookMetricsError::OutcomeTotalMismatch {
            invocations: raw.invocations,
            outcomes: outcome_total,
        });
    }

    let duration_total = raw
        .duration_ms
        .checked_total()
        .ok_or(HookMetricsError::DurationTotalOverflow)?;
    if duration_total != raw.invocations {
        return Err(HookMetricsError::DurationTotalMismatch {
            invocations: raw.invocations,
            durations: duration_total,
        });
    }

    if raw.plugins_completed > raw.plugins_attempted {
        return Err(HookMetricsError::CompletedPluginsExceedAttempts {
            attempted: raw.plugins_attempted,
            completed: raw.plugins_completed,
        });
    }

    validate_session_counts(SessionCountInput {
        counts_complete: raw.session_counts_complete,
        identified_sessions: raw.identified_sessions,
        identified_sessions_non_ok: raw.identified_sessions_non_ok,
        observations: raw.invocations,
        ok_observations: raw.outcomes.ok,
    })?;

    Ok(())
}

/// Invalid relationship between fields in a hook metrics row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookMetricsError {
    NoInvocations,
    OutcomeTotalOverflow,
    OutcomeTotalMismatch { invocations: u64, outcomes: u64 },
    DurationTotalOverflow,
    DurationTotalMismatch { invocations: u64, durations: u64 },
    CompletedPluginsExceedAttempts { attempted: u64, completed: u64 },
    SessionCounts(SessionCountError),
}

impl From<SessionCountError> for HookMetricsError {
    fn from(error: SessionCountError) -> Self {
        Self::SessionCounts(error)
    }
}

impl fmt::Display for HookMetricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoInvocations => formatter.write_str("hook metrics row has no invocations"),
            Self::OutcomeTotalOverflow => formatter.write_str("hook outcome total overflows u64"),
            Self::OutcomeTotalMismatch {
                invocations,
                outcomes,
            } => write!(
                formatter,
                "hook outcome total {outcomes} does not match {invocations} invocations"
            ),
            Self::DurationTotalOverflow => formatter.write_str("hook duration total overflows u64"),
            Self::DurationTotalMismatch {
                invocations,
                durations,
            } => write!(
                formatter,
                "hook duration total {durations} does not match {invocations} invocations"
            ),
            Self::CompletedPluginsExceedAttempts {
                attempted,
                completed,
            } => write!(
                formatter,
                "completed plugin hooks {completed} exceed {attempted} attempted plugin hooks"
            ),
            Self::SessionCounts(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HookMetricsError {}

#[cfg(test)]
mod tests {
    use super::super::{
        IDENTIFIER_WINDOW_TEST_STATE, RowClassification, TelemetryRow, UtcSecond,
        assert_contract_names_with_labels, classify_row, recorded_data_example_row,
        recording_observation,
    };
    use super::*;
    use crate::telemetry::{
        identity::{HookSubject, encode_dimension_for_test},
        state::{HookSessionCountTracker, TelemetryStateV1},
    };
    use chrono::{TimeZone as _, Utc};

    fn metric_observation<'a>(
        outcome: HookOutcome,
        vendor_session_id: Option<&'a VendorSessionId>,
    ) -> HookMetricObservation<'a> {
        HookMetricObservation {
            agent: HookAgent::Claude,
            hook: HookSurface::PreToolUse,
            outcome,
            plugins_attempted: 1,
            plugins_completed: 1,
            duration: Duration::from_millis(5),
            vendor_session_id,
        }
    }

    fn hook_subject(agent: HookAgent, hook: HookSurface) -> HookSubject {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let observation = recording_observation(&mut state);
        let dimension = HookDimension::new(agent, hook);

        observation.identifier_window_scope().derive(&dimension)
    }

    fn session_counts(
        recording: &BoundRecordingObservation<'_>,
        agent: HookAgent,
        hook: HookSurface,
    ) -> HookSessionCountTracker<HookMetricsKey> {
        HookSessionCountTracker::new(HookMetricsKey::new(recording, agent, hook))
    }

    fn hook_metrics_value() -> serde_json::Value {
        serde_json::from_str(recorded_data_example_row("hook_metrics")).unwrap()
    }

    fn hook_metrics_error(value: serde_json::Value) -> String {
        serde_json::from_value::<HookMetricsV1>(value)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn hook_surfaces_round_trip_with_contract_names() {
        let cases = [
            (HookSurface::PreToolUse, "pre_tool_use"),
            (HookSurface::PostToolUse, "post_tool_use"),
            (HookSurface::UserPromptSubmit, "user_prompt_submit"),
            (HookSurface::SessionStart, "session_start"),
            (HookSurface::Stop, "stop"),
        ];

        assert_contract_names_with_labels(&cases, HookSurface::as_str);
    }

    #[test]
    fn sdk_hook_events_map_to_contract_surfaces() {
        let cases = [
            (HookEvent::PreToolUse, HookSurface::PreToolUse),
            (HookEvent::PostToolUse, HookSurface::PostToolUse),
            (HookEvent::UserPromptSubmit, HookSurface::UserPromptSubmit),
            (HookEvent::SessionStart, HookSurface::SessionStart),
            (HookEvent::Stop, HookSurface::Stop),
        ];

        for (event, expected) in cases {
            assert_eq!(HookSurface::try_from(event), Ok(expected));
        }
    }

    #[test]
    fn hook_outcome_counters_round_trip_in_contract_order() {
        let counters = HookOutcomeCounters {
            ok: 1,
            blocked: 2,
            plugin_error: 3,
            internal_error: 4,
        };
        let expected = r#"{"ok":1,"blocked":2,"plugin_error":3,"internal_error":4}"#;

        let json = serde_json::to_string(&counters).unwrap();
        let decoded = serde_json::from_str::<HookOutcomeCounters>(&json).unwrap();

        assert_eq!(json, expected);
        assert_eq!(decoded, counters);
    }

    #[test]
    fn recording_each_hook_outcome_increments_only_its_counter() {
        let cases = [
            (
                HookOutcome::Ok,
                r#"{"ok":1,"blocked":0,"plugin_error":0,"internal_error":0}"#,
            ),
            (
                HookOutcome::Blocked,
                r#"{"ok":0,"blocked":1,"plugin_error":0,"internal_error":0}"#,
            ),
            (
                HookOutcome::PluginError,
                r#"{"ok":0,"blocked":0,"plugin_error":1,"internal_error":0}"#,
            ),
            (
                HookOutcome::InternalError,
                r#"{"ok":0,"blocked":0,"plugin_error":0,"internal_error":1}"#,
            ),
        ];

        for (outcome, expected) in cases {
            let mut counters = HookOutcomeCounters::default();

            counters.checked_record(outcome).unwrap();

            let json = serde_json::to_string(&counters).unwrap();
            let value = serde_json::to_value(counters).unwrap();
            let active_counter = value
                .as_object()
                .unwrap()
                .iter()
                .find_map(|(name, count)| (count.as_u64() == Some(1)).then_some(name.as_str()))
                .unwrap();

            assert_eq!(json, expected);
            assert_eq!(active_counter, outcome.counter_name());
        }
    }

    #[test]
    fn hook_outcome_uses_the_contract_precedence() {
        let cases = [
            (
                HookOutcomeSignals {
                    internal_error: false,
                    blocked: false,
                    plugin_error: false,
                },
                HookOutcome::Ok,
            ),
            (
                HookOutcomeSignals {
                    internal_error: false,
                    blocked: false,
                    plugin_error: true,
                },
                HookOutcome::PluginError,
            ),
            (
                HookOutcomeSignals {
                    internal_error: false,
                    blocked: true,
                    plugin_error: true,
                },
                HookOutcome::Blocked,
            ),
            (
                HookOutcomeSignals {
                    internal_error: true,
                    blocked: true,
                    plugin_error: true,
                },
                HookOutcome::InternalError,
            ),
        ];

        for (signals, expected) in cases {
            assert_eq!(HookOutcome::from_signals(signals), expected);
        }
    }

    #[test]
    fn hook_outcome_counters_reject_missing_unknown_and_invalid_fields() {
        let cases = [
            r#"{"ok":0,"blocked":0,"plugin_error":0}"#,
            r#"{"ok":0,"blocked":0,"plugin_error":0,"internal_error":0,"future":0}"#,
            r#"{"ok":"none","blocked":0,"plugin_error":0,"internal_error":0}"#,
            r#"{"ok":-1,"blocked":0,"plugin_error":0,"internal_error":0}"#,
        ];

        for json in cases {
            assert!(
                serde_json::from_str::<HookOutcomeCounters>(json).is_err(),
                "accepted invalid hook outcome counters {json}"
            );
        }
    }

    #[test]
    fn recording_an_outcome_rejects_overflow_without_mutation() {
        let outcomes = [
            HookOutcome::Ok,
            HookOutcome::Blocked,
            HookOutcome::PluginError,
            HookOutcome::InternalError,
        ];

        for outcome in outcomes {
            let mut counters = HookOutcomeCounters {
                ok: u64::MAX,
                blocked: u64::MAX,
                plugin_error: u64::MAX,
                internal_error: u64::MAX,
            };
            let before = counters;

            let result = counters.checked_record(outcome);

            assert_eq!(result, Err(HookOutcomeCounterOverflow { outcome }));
            assert_eq!(counters, before);
        }
    }

    #[test]
    fn hook_outcome_counter_total_is_checked_for_overflow() {
        let representable = HookOutcomeCounters {
            ok: 1,
            blocked: 2,
            plugin_error: 3,
            internal_error: 4,
        };
        let overflowing = HookOutcomeCounters {
            ok: u64::MAX,
            blocked: 1,
            plugin_error: 0,
            internal_error: 0,
        };

        assert_eq!(representable.checked_total(), Some(10));
        assert_eq!(overflowing.checked_total(), None);
    }

    #[test]
    fn hook_surface_rejects_unknown_contract_names() {
        let result = serde_json::from_str::<HookSurface>(r#""before_tool_use""#);

        assert!(result.is_err());
    }

    #[test]
    fn hook_subject_dimension_uses_agent_then_hook_surface() {
        let dimension = HookDimension::new(HookAgent::Claude, HookSurface::PreToolUse);
        let expected = [
            [0, 0, 0, 0, 0, 0, 0, 6].as_slice(),
            b"claude".as_slice(),
            [0, 0, 0, 0, 0, 0, 0, 12].as_slice(),
            b"pre_tool_use".as_slice(),
        ]
        .concat();

        let encoded = encode_dimension_for_test(&dimension);

        assert_eq!(encoded, expected);
    }

    #[test]
    fn hook_subject_derivation_matches_independent_vector() {
        // Cross-checked with .NET's HMACSHA256 over the contract header,
        // identifier window, agent, and hook surface. The complete digest is
        // 99106b457209912cf0023c562a72dafb225707f0718f26e3a379c42fedf1cf55.
        let expected = "hok_99106b457209912cf0023c562a72dafb".parse().unwrap();

        let subject = hook_subject(HookAgent::Claude, HookSurface::PreToolUse);

        assert_eq!(subject, expected);
    }

    #[test]
    fn hook_subject_changes_with_agent_or_hook_surface() {
        let baseline = hook_subject(HookAgent::Claude, HookSurface::PreToolUse);
        let other_agent = hook_subject(HookAgent::Codex, HookSurface::PreToolUse);
        let other_surface = hook_subject(HookAgent::Claude, HookSurface::PostToolUse);

        assert_ne!(baseline, other_agent);
        assert_ne!(baseline, other_surface);
    }

    #[test]
    fn first_hook_observation_populates_the_complete_aggregate() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let input = metric_observation(HookOutcome::Blocked, Some(&vendor_session_id));

        let row = HookMetricsV1::new(&recording, input, &mut session_counts).unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();

        assert_eq!(row.version, SchemaVersion::V1);
        assert_eq!(row.kind, RowKind::HookMetrics);
        assert_eq!(row.event_id.0.get_version(), Some(uuid::Version::Random));
        assert_eq!(row.day, recording.day());
        assert_eq!(row.symposium, SymposiumVersion::current());
        assert_eq!(row.agent, HookAgent::Claude);
        assert_eq!(row.hook, HookSurface::PreToolUse);
        assert_eq!(row.invocations, 1);
        assert_eq!(
            value["outcomes"],
            serde_json::json!({
                "ok": 0,
                "blocked": 1,
                "plugin_error": 0,
                "internal_error": 0,
            })
        );
        assert_eq!(row.plugins_attempted, 1);
        assert_eq!(row.plugins_completed, 1);
        assert_eq!(
            value["duration_ms"]["counts"],
            serde_json::json!([1, 0, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(row.identified_sessions, Some(1));
        assert_eq!(row.identified_sessions_non_ok, Some(1));
        assert!(row.session_counts_complete);
        assert_eq!(
            row.hook_subject,
            "hok_99106b457209912cf0023c562a72dafb".parse().unwrap()
        );
        assert!(matches!(
            classify_row(&json),
            RowClassification::Supported(TelemetryRow::HookMetrics(_))
        ));
    }

    #[test]
    fn later_hook_observations_accumulate_every_metric() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let first_session = VendorSessionId::new("vendor-session-123".to_owned());
        let second_session = VendorSessionId::new("vendor-session-456".to_owned());
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let mut row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&first_session)),
            &mut session_counts,
        )
        .unwrap();
        let event_id = row.event_id;
        let second = HookMetricObservation {
            plugins_attempted: 3,
            plugins_completed: 2,
            duration: Duration::from_millis(1_001),
            ..metric_observation(HookOutcome::Blocked, Some(&second_session))
        };

        row.checked_record(&recording, second, &mut session_counts)
            .unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();

        assert_eq!(row.event_id, event_id);
        assert_eq!(row.invocations, 2);
        assert_eq!(
            value["outcomes"],
            serde_json::json!({
                "ok": 1,
                "blocked": 1,
                "plugin_error": 0,
                "internal_error": 0,
            })
        );
        assert_eq!(row.plugins_attempted, 4);
        assert_eq!(row.plugins_completed, 3);
        assert_eq!(
            value["duration_ms"]["counts"],
            serde_json::json!([1, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(row.identified_sessions, Some(2));
        assert_eq!(row.identified_sessions_non_ok, Some(1));
        assert!(row.session_counts_complete);
        assert!(matches!(
            classify_row(&json),
            RowClassification::Supported(TelemetryRow::HookMetrics(_))
        ));
    }

    #[test]
    fn missing_session_id_makes_the_first_aggregate_incomplete() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);

        let row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, None),
            &mut session_counts,
        )
        .unwrap();
        let value = serde_json::to_value(row).unwrap();

        assert_eq!(value["session_counts_complete"], false);
        assert!(value.get("identified_sessions").is_none());
        assert!(value.get("identified_sessions_non_ok").is_none());
    }

    #[test]
    fn snapshot_mismatch_makes_session_counts_incomplete_during_row_update() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let mut row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut session_counts,
        )
        .unwrap();
        let session_id = derive_session_id(
            recording.identifier_window_scope(),
            HookAgent::Claude,
            Some(&vendor_session_id),
        );
        session_counts
            .checked_record(1, session_id, HookOutcome::Ok)
            .unwrap();

        row.checked_record(
            &recording,
            metric_observation(HookOutcome::Blocked, Some(&vendor_session_id)),
            &mut session_counts,
        )
        .unwrap();

        assert_eq!(row.invocations, 2);
        assert!(!row.session_counts_complete);
        assert_eq!(row.identified_sessions, None);
        assert_eq!(row.identified_sessions_non_ok, None);
    }

    #[test]
    fn failed_histogram_update_preserves_the_row_and_session_tracker() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let mut row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut session_counts,
        )
        .unwrap();
        row.duration_ms = serde_json::from_value(serde_json::json!({
            "bounds": [5, 10, 25, 50, 100, 250, 500, 1000],
            "counts": [u64::MAX, 0, 0, 0, 0, 0, 0, 0, 0],
        }))
        .unwrap();
        let row_before = row.clone();
        let session_counts_before = session_counts.clone();

        let result = row.checked_record(
            &recording,
            metric_observation(HookOutcome::Blocked, Some(&vendor_session_id)),
            &mut session_counts,
        );

        assert_eq!(
            result,
            Err(HookMetricsUpdateError::Duration(
                LatencyHistogramError::BucketCountOverflow { bucket: 0 }
            ))
        );
        assert_eq!(row, row_before);
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn aggregate_rejects_an_observation_for_another_target_without_mutation() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let mut row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut session_counts,
        )
        .unwrap();
        let row_before = row.clone();
        let session_counts_before = session_counts.clone();
        let wrong_target = HookMetricObservation {
            agent: HookAgent::Codex,
            ..metric_observation(HookOutcome::Ok, Some(&vendor_session_id))
        };

        let result = row.checked_record(&recording, wrong_target, &mut session_counts);

        assert_eq!(result, Err(HookMetricsUpdateError::TargetChanged));
        assert_eq!(row, row_before);
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn aggregate_rejects_an_observation_from_another_day_without_mutation() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let (mut row, mut session_counts) = {
            let recording = recording_observation(&mut state);
            let mut session_counts =
                session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
            let row = HookMetricsV1::new(
                &recording,
                metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
                &mut session_counts,
            )
            .unwrap();
            (row, session_counts)
        };
        let completed_at =
            UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 8, 4, 10, 2, 11).unwrap());
        let later = state.observe_recording(completed_at).unwrap();
        let later = state.bind_recording_observation(later).unwrap();
        let row_before = row.clone();
        let session_counts_before = session_counts.clone();

        let result = row.checked_record(
            &later,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut session_counts,
        );

        assert_eq!(
            result,
            Err(HookMetricsUpdateError::DayChanged {
                row_day: row_before.day,
                observation_day: later.day(),
            })
        );
        assert_eq!(row, row_before);
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn aggregate_rejects_another_identifier_epoch_without_mutation() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let (mut row, mut session_counts, completed_at) = {
            let recording = recording_observation(&mut state);
            let mut session_counts =
                session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
            let row = HookMetricsV1::new(
                &recording,
                metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
                &mut session_counts,
            )
            .unwrap();
            (row, session_counts, recording.completed_at())
        };
        state.reset_identifiers(row.day).unwrap();
        let recording = state.observe_recording(completed_at).unwrap();
        let recording = state.bind_recording_observation(recording).unwrap();
        let row_before = row.clone();
        let session_counts_before = session_counts.clone();

        let result = row.checked_record(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut session_counts,
        );

        assert_eq!(result, Err(HookMetricsUpdateError::IdentifierEpochChanged));
        assert_eq!(row, row_before);
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn aggregate_rejects_a_tracker_for_another_hook_surface() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
        let mut pre_tool_tracker =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let mut post_tool_tracker =
            session_counts(&recording, HookAgent::Claude, HookSurface::PostToolUse);
        let mut pre_tool_row = HookMetricsV1::new(
            &recording,
            metric_observation(HookOutcome::Ok, Some(&vendor_session_id)),
            &mut pre_tool_tracker,
        )
        .unwrap();
        let post_tool_observation = HookMetricObservation {
            hook: HookSurface::PostToolUse,
            ..metric_observation(HookOutcome::Ok, Some(&vendor_session_id))
        };
        let _post_tool_row =
            HookMetricsV1::new(&recording, post_tool_observation, &mut post_tool_tracker).unwrap();
        let row_before = pre_tool_row.clone();
        let tracker_before = post_tool_tracker.clone();

        let result = pre_tool_row.checked_record(
            &recording,
            metric_observation(HookOutcome::Blocked, Some(&vendor_session_id)),
            &mut post_tool_tracker,
        );

        assert_eq!(result, Err(HookMetricsUpdateError::SessionTrackerChanged));
        assert_eq!(pre_tool_row, row_before);
        assert_eq!(post_tool_tracker, tracker_before);
    }

    #[test]
    fn invalid_plugin_counts_reject_the_first_observation_without_state_change() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let session_counts_before = session_counts.clone();
        let input = HookMetricObservation {
            plugins_attempted: 0,
            plugins_completed: 1,
            ..metric_observation(HookOutcome::Ok, None)
        };

        let result = HookMetricsV1::new(&recording, input, &mut session_counts);

        assert_eq!(
            result,
            Err(HookMetricsUpdateError::CompletedPluginsExceedAttempts {
                attempted: 0,
                completed: 1,
            })
        );
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn invalid_plugin_counts_are_rejected_even_when_the_row_has_slack() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let mut session_counts =
            session_counts(&recording, HookAgent::Claude, HookSurface::PreToolUse);
        let first = HookMetricObservation {
            plugins_attempted: 5,
            plugins_completed: 3,
            ..metric_observation(HookOutcome::Ok, None)
        };
        let mut row = HookMetricsV1::new(&recording, first, &mut session_counts).unwrap();
        let row_before = row.clone();
        let session_counts_before = session_counts.clone();
        let impossible = HookMetricObservation {
            plugins_attempted: 0,
            plugins_completed: 2,
            ..metric_observation(HookOutcome::Ok, None)
        };

        let result = row.checked_record(&recording, impossible, &mut session_counts);

        assert_eq!(
            result,
            Err(HookMetricsUpdateError::CompletedPluginsExceedAttempts {
                attempted: 0,
                completed: 2,
            })
        );
        assert_eq!(row, row_before);
        assert_eq!(session_counts, session_counts_before);
    }

    #[test]
    fn hook_metrics_example_round_trips_through_the_classifier() {
        let source = recorded_data_example_row("hook_metrics");

        let RowClassification::Supported(TelemetryRow::HookMetrics(row)) = classify_row(source)
        else {
            panic!("documented hook metrics row was not classified as supported");
        };
        let serialized = serde_json::to_string(&row).unwrap();

        assert_eq!(serialized, source);
    }

    #[test]
    fn hook_metrics_rejects_future_versions_unknown_fields_and_missing_fields() {
        let mut future_version = hook_metrics_value();
        future_version["v"] = serde_json::Value::from(2);
        let future_json = serde_json::to_string(&future_version).unwrap();
        let mut unknown_field = hook_metrics_value();
        unknown_field["future"] = serde_json::Value::Bool(true);
        let mut missing_field = hook_metrics_value();
        missing_field
            .as_object_mut()
            .unwrap()
            .remove("plugins_attempted");

        assert_eq!(classify_row(&future_json), RowClassification::UnknownSchema);
        for value in [future_version, unknown_field, missing_field] {
            assert!(serde_json::from_value::<HookMetricsV1>(value).is_err());
        }
    }

    #[test]
    fn hook_metrics_rejects_an_outcome_total_that_differs_from_invocations() {
        let mut value = hook_metrics_value();
        value["outcomes"]["ok"] = serde_json::Value::from(497);
        let json = serde_json::to_string(&value).unwrap();

        let error = hook_metrics_error(value);

        assert!(error.contains("hook outcome total 499 does not match 500 invocations"));
        assert_eq!(classify_row(&json), RowClassification::Invalid);
    }

    #[test]
    fn hook_metrics_rejects_an_overflowing_outcome_total() {
        let mut value = hook_metrics_value();
        value["outcomes"]["ok"] = serde_json::Value::from(u64::MAX);

        let error = hook_metrics_error(value);

        assert!(error.contains("hook outcome total overflows u64"));
    }

    #[test]
    fn hook_metrics_rejects_a_duration_total_that_differs_from_invocations() {
        let mut value = hook_metrics_value();
        value["duration_ms"]["counts"][7] = serde_json::Value::from(0);

        let error = hook_metrics_error(value);

        assert!(error.contains("hook duration total 499 does not match 500 invocations"));
    }

    #[test]
    fn hook_metrics_rejects_an_overflowing_duration_total() {
        let mut value = hook_metrics_value();
        value["duration_ms"]["counts"][0] = serde_json::Value::from(u64::MAX);

        let error = hook_metrics_error(value);

        assert!(error.contains("hook duration total overflows u64"));
    }

    #[test]
    fn hook_metrics_rejects_more_completed_than_attempted_plugins() {
        let mut value = hook_metrics_value();
        value["plugins_completed"] = serde_json::Value::from(501);

        let error = hook_metrics_error(value);

        assert!(error.contains("completed plugin hooks 501 exceed 500 attempted plugin hooks"));
    }

    #[test]
    fn complete_hook_metrics_requires_both_session_counts() {
        for missing in ["identified_sessions", "identified_sessions_non_ok"] {
            let mut value = hook_metrics_value();
            value.as_object_mut().unwrap().remove(missing);

            let error = hook_metrics_error(value);

            assert!(error.contains("require both identified session counters"));
        }
    }

    #[test]
    fn incomplete_hook_metrics_omits_both_session_counts() {
        let mut value = hook_metrics_value();
        value["session_counts_complete"] = serde_json::Value::Bool(false);
        value.as_object_mut().unwrap().remove("identified_sessions");
        value
            .as_object_mut()
            .unwrap()
            .remove("identified_sessions_non_ok");

        let row = serde_json::from_value::<HookMetricsV1>(value).unwrap();
        let serialized = serde_json::to_value(row).unwrap();

        assert_eq!(serialized["session_counts_complete"], false);
        assert!(serialized.get("identified_sessions").is_none());
        assert!(serialized.get("identified_sessions_non_ok").is_none());
    }

    #[test]
    fn incomplete_hook_metrics_rejects_either_session_count() {
        for present in ["identified_sessions", "identified_sessions_non_ok"] {
            let mut value = hook_metrics_value();
            value["session_counts_complete"] = serde_json::Value::Bool(false);
            value.as_object_mut().unwrap().remove("identified_sessions");
            value
                .as_object_mut()
                .unwrap()
                .remove("identified_sessions_non_ok");
            value[present] = serde_json::Value::from(1);

            let error = hook_metrics_error(value);

            assert!(error.contains("must omit both identified session counters"));
        }
    }

    #[test]
    fn hook_metrics_rejects_more_non_ok_than_identified_sessions() {
        let mut value = hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(1);
        value["identified_sessions_non_ok"] = serde_json::Value::from(2);

        let error = hook_metrics_error(value);

        assert!(error.contains("non-ok identified sessions 2 exceed 1 identified sessions"));
    }

    #[test]
    fn hook_metrics_rejects_more_than_256_identified_sessions() {
        let mut value = hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(257);

        let error = hook_metrics_error(value);

        assert!(error.contains("identified sessions 257 exceed the version 1 limit 256"));
    }

    #[test]
    fn hook_metrics_rejects_more_identified_sessions_than_invocations() {
        let mut value = hook_metrics_value();
        value["invocations"] = serde_json::Value::from(1);
        value["outcomes"] = serde_json::json!({
            "ok": 0,
            "blocked": 1,
            "plugin_error": 0,
            "internal_error": 0,
        });
        value["duration_ms"]["counts"] = serde_json::json!([1, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["identified_sessions"] = serde_json::Value::from(2);
        value["identified_sessions_non_ok"] = serde_json::Value::from(1);

        let error = hook_metrics_error(value);

        assert!(error.contains("identified sessions 2 exceed 1 observations"));
    }

    #[test]
    fn hook_metrics_rejects_more_non_ok_sessions_than_non_ok_invocations() {
        let mut value = hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(3);
        value["identified_sessions_non_ok"] = serde_json::Value::from(3);

        let error = hook_metrics_error(value);

        assert!(error.contains("non-ok identified sessions 3 exceed 2 non-ok observations"));
    }

    #[test]
    fn hook_metrics_rejects_an_empty_aggregate() {
        let mut value = hook_metrics_value();
        value["invocations"] = serde_json::Value::from(0);
        value["outcomes"] = serde_json::json!({
            "ok": 0,
            "blocked": 0,
            "plugin_error": 0,
            "internal_error": 0,
        });
        value["plugins_attempted"] = serde_json::Value::from(0);
        value["plugins_completed"] = serde_json::Value::from(0);
        value["duration_ms"]["counts"] = serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["session_counts_complete"] = serde_json::Value::Bool(false);
        value.as_object_mut().unwrap().remove("identified_sessions");
        value
            .as_object_mut()
            .unwrap()
            .remove("identified_sessions_non_ok");

        let error = hook_metrics_error(value);

        assert!(error.contains("hook metrics row has no invocations"));
    }

    #[test]
    fn complete_hook_metrics_requires_at_least_one_identified_session() {
        let mut value = hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(0);
        value["identified_sessions_non_ok"] = serde_json::Value::from(0);

        let error = hook_metrics_error(value);

        assert!(error.contains("complete session counts contain no identified sessions"));
    }

    #[test]
    fn non_ok_invocations_require_at_least_one_non_ok_session() {
        let mut value = hook_metrics_value();
        value["identified_sessions_non_ok"] = serde_json::Value::from(0);

        let error = hook_metrics_error(value);

        assert!(
            error.contains("2 non-ok observations require at least one non-ok identified session")
        );
    }

    #[test]
    fn hook_metrics_rejects_more_all_ok_sessions_than_ok_invocations() {
        let mut value = hook_metrics_value();
        value["invocations"] = serde_json::Value::from(3);
        value["outcomes"] = serde_json::json!({
            "ok": 1,
            "blocked": 2,
            "plugin_error": 0,
            "internal_error": 0,
        });
        value["duration_ms"]["counts"] = serde_json::json!([3, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["identified_sessions"] = serde_json::Value::from(3);
        value["identified_sessions_non_ok"] = serde_json::Value::from(1);

        let error = hook_metrics_error(value);

        assert!(error.contains("all-ok identified sessions 2 exceed 1 ok observations"));
    }

    #[test]
    fn hook_metrics_accepts_exact_session_count_limits() {
        let mut value = hook_metrics_value();
        value["invocations"] = serde_json::Value::from(256);
        value["outcomes"] = serde_json::json!({
            "ok": 254,
            "blocked": 2,
            "plugin_error": 0,
            "internal_error": 0,
        });
        value["duration_ms"]["counts"] = serde_json::json!([256, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["identified_sessions"] = serde_json::Value::from(256);
        value["identified_sessions_non_ok"] = serde_json::Value::from(2);
        let json = serde_json::to_string(&value).unwrap();

        let classification = classify_row(&json);

        assert!(matches!(
            classification,
            RowClassification::Supported(TelemetryRow::HookMetrics(_))
        ));
    }
}
