//! Hook aggregate vocabulary, counters, rows, and identity dimensions.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    RowKind, SymposiumVersion, agent::HookAgent, macros::strict_versioned_row,
    metrics::LatencyHistogram,
};
use crate::{
    hook_schema::HookEvent,
    telemetry::identity::{DimensionWriter, HookDomain, HookSubject, IdentityDimension},
};

const MAX_IDENTIFIED_SESSIONS: u64 = 256;

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
    error: HookMetricsError,
    validate: validate_hook_metrics,
}

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

    match (
        raw.session_counts_complete,
        raw.identified_sessions,
        raw.identified_sessions_non_ok,
    ) {
        (true, Some(identified), Some(non_ok)) => {
            if identified == 0 {
                return Err(HookMetricsError::NoIdentifiedSessions);
            }
            if identified > MAX_IDENTIFIED_SESSIONS {
                return Err(HookMetricsError::IdentifiedSessionsExceedLimit {
                    identified,
                    maximum: MAX_IDENTIFIED_SESSIONS,
                });
            }
            if identified > raw.invocations {
                return Err(HookMetricsError::IdentifiedSessionsExceedInvocations {
                    identified,
                    invocations: raw.invocations,
                });
            }
            if non_ok > identified {
                return Err(HookMetricsError::NonOkSessionsExceedIdentified { identified, non_ok });
            }

            // The outcome-total check above proves that `ok` cannot exceed
            // `invocations`, so this subtraction cannot underflow.
            let non_ok_invocations = raw.invocations - raw.outcomes.ok;
            if non_ok_invocations > 0 && non_ok == 0 {
                return Err(HookMetricsError::NoNonOkSessions {
                    invocations: non_ok_invocations,
                });
            }
            if non_ok > non_ok_invocations {
                return Err(HookMetricsError::NonOkSessionsExceedNonOkInvocations {
                    sessions: non_ok,
                    invocations: non_ok_invocations,
                });
            }

            // The subset check above proves that this subtraction cannot
            // underflow. Every remaining session contributed an `ok` result.
            let all_ok_sessions = identified - non_ok;
            if all_ok_sessions > raw.outcomes.ok {
                return Err(HookMetricsError::AllOkSessionsExceedOkInvocations {
                    sessions: all_ok_sessions,
                    invocations: raw.outcomes.ok,
                });
            }

            Ok(())
        }
        (false, None, None) => Ok(()),
        (true, _, _) => Err(HookMetricsError::CompleteSessionCountsMissing),
        (false, _, _) => Err(HookMetricsError::IncompleteSessionCountsPresent),
    }
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
    CompleteSessionCountsMissing,
    IncompleteSessionCountsPresent,
    NoIdentifiedSessions,
    NoNonOkSessions { invocations: u64 },
    IdentifiedSessionsExceedLimit { identified: u64, maximum: u64 },
    IdentifiedSessionsExceedInvocations { identified: u64, invocations: u64 },
    NonOkSessionsExceedIdentified { identified: u64, non_ok: u64 },
    NonOkSessionsExceedNonOkInvocations { sessions: u64, invocations: u64 },
    AllOkSessionsExceedOkInvocations { sessions: u64, invocations: u64 },
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
            Self::CompleteSessionCountsMissing => formatter
                .write_str("complete hook session counts require both identified session counters"),
            Self::IncompleteSessionCountsPresent => formatter.write_str(
                "incomplete hook session counts must omit both identified session counters",
            ),
            Self::NoIdentifiedSessions => {
                formatter.write_str("complete hook session counts contain no identified sessions")
            }
            Self::NoNonOkSessions { invocations } => write!(
                formatter,
                "{invocations} non-ok hook invocations require at least one non-ok identified session"
            ),
            Self::IdentifiedSessionsExceedLimit {
                identified,
                maximum,
            } => write!(
                formatter,
                "identified sessions {identified} exceed the version 1 limit {maximum}"
            ),
            Self::IdentifiedSessionsExceedInvocations {
                identified,
                invocations,
            } => write!(
                formatter,
                "identified sessions {identified} exceed {invocations} hook invocations"
            ),
            Self::NonOkSessionsExceedIdentified { identified, non_ok } => write!(
                formatter,
                "non-ok identified sessions {non_ok} exceed {identified} identified sessions"
            ),
            Self::NonOkSessionsExceedNonOkInvocations {
                sessions,
                invocations,
            } => write!(
                formatter,
                "non-ok identified sessions {sessions} exceed {invocations} non-ok hook invocations"
            ),
            Self::AllOkSessionsExceedOkInvocations {
                sessions,
                invocations,
            } => write!(
                formatter,
                "all-ok identified sessions {sessions} exceed {invocations} ok hook invocations"
            ),
        }
    }
}

impl std::error::Error for HookMetricsError {}

#[cfg(test)]
mod tests {
    use super::super::{
        IDENTIFIER_WINDOW_TEST_STATE, RowClassification, TelemetryRow,
        assert_contract_names_with_labels, classify_row, recorded_data_example_row,
        recording_observation,
    };
    use super::*;
    use crate::telemetry::{
        identity::{HookSubject, encode_dimension_for_test},
        state::TelemetryStateV1,
    };

    fn hook_subject(agent: HookAgent, hook: HookSurface) -> HookSubject {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let observation = recording_observation(&mut state);
        let dimension = HookDimension::new(agent, hook);

        observation.identifier_window_scope().derive(&dimension)
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

        assert!(error.contains("identified sessions 2 exceed 1 hook invocations"));
    }

    #[test]
    fn hook_metrics_rejects_more_non_ok_sessions_than_non_ok_invocations() {
        let mut value = hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(3);
        value["identified_sessions_non_ok"] = serde_json::Value::from(3);

        let error = hook_metrics_error(value);

        assert!(error.contains("non-ok identified sessions 3 exceed 2 non-ok hook invocations"));
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

        assert!(error.contains("complete hook session counts contain no identified sessions"));
    }

    #[test]
    fn non_ok_invocations_require_at_least_one_non_ok_session() {
        let mut value = hook_metrics_value();
        value["identified_sessions_non_ok"] = serde_json::Value::from(0);

        let error = hook_metrics_error(value);

        assert!(
            error.contains(
                "2 non-ok hook invocations require at least one non-ok identified session"
            )
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

        assert!(error.contains("all-ok identified sessions 2 exceed 1 ok hook invocations"));
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
