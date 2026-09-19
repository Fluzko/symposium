//! Plugin-hook aggregate vocabulary and public identity.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    RowKind, SymposiumVersion,
    agent::HookAgent,
    extension::{
        ExtensionKind, PublicExtensionCoordinate, PublicExtensionName, PublicExtensionNameError,
        PublicExtensionSource,
    },
    hook::HookSurface,
    macros::strict_versioned_row,
    metrics::{LatencyHistogram, SessionCountError, SessionCountInput, validate_session_counts},
};
use crate::telemetry::identity::{DimensionWriter, IdentityDimension, PluginDomain, PluginSubject};

/// Identity exposure assigned to one plugin-hook aggregate bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::telemetry) enum PluginScope {
    Public,
    Unnamed,
    Overflow,
}

impl PluginScope {
    /// Return the frozen version 1 wire label.
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unnamed => "unnamed",
            Self::Overflow => "overflow",
        }
    }
}

/// Final outcome assigned to one completed plugin-hook attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum PluginHookOutcome {
    Ok,
    Blocked,
    Error,
}

impl PluginHookOutcome {
    /// Select the final outcome using the version 1 precedence rule.
    #[must_use]
    pub(in crate::telemetry) const fn from_signals(signals: PluginHookOutcomeSignals) -> Self {
        if signals.error {
            Self::Error
        } else if signals.blocked {
            Self::Blocked
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
            Self::Error => "error",
        }
    }
}

/// Signals used to select one final plugin-hook outcome.
///
/// Named fields prevent the precedence inputs from being transposed at call
/// sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct PluginHookOutcomeSignals {
    pub(in crate::telemetry) error: bool,
    pub(in crate::telemetry) blocked: bool,
}

/// Mutually exclusive outcomes of completed plugin-hook attempts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginHookOutcomeCounters {
    ok: u64,
    blocked: u64,
    error: u64,
}

impl PluginHookOutcomeCounters {
    /// Increment exactly one outcome counter.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHookOutcomeCounterOverflow`] when the selected counter
    /// cannot be incremented. The counters are unchanged on failure.
    #[must_use = "counter overflow must drop the containing telemetry update"]
    fn checked_record(
        &mut self,
        outcome: PluginHookOutcome,
    ) -> Result<(), PluginHookOutcomeCounterOverflow> {
        let counter = match outcome {
            PluginHookOutcome::Ok => &mut self.ok,
            PluginHookOutcome::Blocked => &mut self.blocked,
            PluginHookOutcome::Error => &mut self.error,
        };
        let next = counter
            .checked_add(1)
            .ok_or(PluginHookOutcomeCounterOverflow { outcome })?;

        *counter = next;
        Ok(())
    }

    /// Return the sum of every outcome counter, or `None` on overflow.
    #[must_use]
    fn checked_total(&self) -> Option<u64> {
        [self.ok, self.blocked, self.error]
            .into_iter()
            .try_fold(0_u64, u64::checked_add)
    }
}

/// A selected plugin-hook outcome counter that cannot be incremented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PluginHookOutcomeCounterOverflow {
    outcome: PluginHookOutcome,
}

impl fmt::Display for PluginHookOutcomeCounterOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} plugin-hook outcome counter overflows u64",
            self.outcome.counter_name()
        )
    }
}

impl std::error::Error for PluginHookOutcomeCounterOverflow {}

/// Public plugin coordinate safe to place in plugin-hook telemetry.
///
/// The enclosing row establishes that this coordinate identifies a plugin,
/// so its wire form contains only the reviewed public source and validated
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::telemetry) struct PublicPluginCoordinate {
    source: PublicExtensionSource,
    name: PublicExtensionName,
}

impl PublicPluginCoordinate {
    /// Combine validated components into one public plugin coordinate.
    #[must_use]
    pub(in crate::telemetry) const fn new(
        source: PublicExtensionSource,
        name: PublicExtensionName,
    ) -> Self {
        Self { source, name }
    }

    /// Validate a raw public name and combine it with an allowlisted source.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is outside the version 1 public extension
    /// grammar.
    pub(in crate::telemetry) fn try_new(
        source: PublicExtensionSource,
        name: &str,
    ) -> Result<Self, PublicExtensionNameError> {
        Ok(Self::new(source, name.parse()?))
    }

    /// Return the reviewed public source.
    #[must_use]
    pub(in crate::telemetry) const fn source(&self) -> PublicExtensionSource {
        self.source
    }

    /// Return the validated public plugin name.
    #[must_use]
    pub(in crate::telemetry) const fn name(&self) -> &PublicExtensionName {
        &self.name
    }
}

impl TryFrom<&PublicExtensionCoordinate> for PublicPluginCoordinate {
    type Error = NotPublicPlugin;

    fn try_from(coordinate: &PublicExtensionCoordinate) -> Result<Self, Self::Error> {
        if coordinate.kind() != ExtensionKind::Plugin {
            return Err(NotPublicPlugin {
                found: coordinate.kind(),
            });
        }

        Ok(Self::new(coordinate.source(), coordinate.name().clone()))
    }
}

impl IdentityDimension for PublicPluginCoordinate {
    type Domain = PluginDomain;

    /// Write public source and plugin name in version 1 contract order.
    fn write(&self, writer: &mut DimensionWriter<'_>) {
        writer.field(self.source.as_str().as_bytes());
        writer.field(self.name.as_str().as_bytes());
    }
}

/// A public extension coordinate that identifies something other than a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct NotPublicPlugin {
    found: ExtensionKind,
}

impl fmt::Display for NotPublicPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "expected a public plugin coordinate, found {}",
            self.found.as_str()
        )
    }
}

impl std::error::Error for NotPublicPlugin {}

strict_versioned_row! {
    /// Version 1 daily aggregate for one plugin, agent, and hook surface.
    pub(in crate::telemetry) struct PluginHookMetricsV1 {
        symposium: SymposiumVersion,
        agent: HookAgent,
        hook: HookSurface,
        plugin_scope: PluginScope,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<PublicPluginCoordinate>,
        attempts: u64,
        executions: u64,
        outcomes: PluginHookOutcomeCounters,
        prepare_ms: LatencyHistogram,
        execute_ms: LatencyHistogram,
        #[serde(skip_serializing_if = "Option::is_none")]
        identified_sessions: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        identified_sessions_non_ok: Option<u64>,
        session_counts_complete: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin_subject: Option<PluginSubject>,
    }

    kind: RowKind::PluginHookMetrics,
    raw: RawPluginHookMetricsV1,
    error: PluginHookMetricsError,
    validate: validate_plugin_hook_metrics,
}

fn validate_plugin_hook_metrics(
    raw: &RawPluginHookMetricsV1,
) -> Result<(), PluginHookMetricsError> {
    if raw.attempts == 0 {
        return Err(PluginHookMetricsError::NoAttempts);
    }

    let outcome_total = raw
        .outcomes
        .checked_total()
        .ok_or(PluginHookMetricsError::OutcomeTotalOverflow)?;
    if outcome_total != raw.attempts {
        return Err(PluginHookMetricsError::OutcomeTotalMismatch {
            attempts: raw.attempts,
            outcomes: outcome_total,
        });
    }

    let prepare_total = raw
        .prepare_ms
        .checked_total()
        .ok_or(PluginHookMetricsError::PrepareDurationTotalOverflow)?;
    if prepare_total != raw.attempts {
        return Err(PluginHookMetricsError::PrepareDurationTotalMismatch {
            attempts: raw.attempts,
            durations: prepare_total,
        });
    }

    if raw.executions > raw.attempts {
        return Err(PluginHookMetricsError::ExecutionsExceedAttempts {
            attempts: raw.attempts,
            executions: raw.executions,
        });
    }

    // The subset check above proves that this subtraction cannot underflow.
    // Every attempt that stopped before child execution has an error outcome.
    let non_executed = raw.attempts - raw.executions;
    if non_executed > raw.outcomes.error {
        return Err(
            PluginHookMetricsError::NonExecutedAttemptsExceedErrorOutcomes {
                non_executed,
                errors: raw.outcomes.error,
            },
        );
    }

    let execute_total = raw
        .execute_ms
        .checked_total()
        .ok_or(PluginHookMetricsError::ExecuteDurationTotalOverflow)?;
    if execute_total != raw.executions {
        return Err(PluginHookMetricsError::ExecuteDurationTotalMismatch {
            executions: raw.executions,
            durations: execute_total,
        });
    }

    let plugin_present = raw.plugin.is_some();
    let subject_present = raw.plugin_subject.is_some();
    let identity_matches_scope = match raw.plugin_scope {
        PluginScope::Public => plugin_present && subject_present,
        PluginScope::Unnamed | PluginScope::Overflow => !plugin_present && !subject_present,
    };
    if !identity_matches_scope {
        return Err(PluginHookMetricsError::PluginIdentityDoesNotMatchScope {
            scope: raw.plugin_scope,
            plugin_present,
            subject_present,
        });
    }

    validate_session_counts(SessionCountInput {
        counts_complete: raw.session_counts_complete,
        identified_sessions: raw.identified_sessions,
        identified_sessions_non_ok: raw.identified_sessions_non_ok,
        observations: raw.attempts,
        ok_observations: raw.outcomes.ok,
    })?;

    Ok(())
}

/// Invalid relationship between fields in a plugin-hook metrics row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginHookMetricsError {
    NoAttempts,
    OutcomeTotalOverflow,
    OutcomeTotalMismatch {
        attempts: u64,
        outcomes: u64,
    },
    PrepareDurationTotalOverflow,
    PrepareDurationTotalMismatch {
        attempts: u64,
        durations: u64,
    },
    ExecutionsExceedAttempts {
        attempts: u64,
        executions: u64,
    },
    NonExecutedAttemptsExceedErrorOutcomes {
        non_executed: u64,
        errors: u64,
    },
    ExecuteDurationTotalOverflow,
    ExecuteDurationTotalMismatch {
        executions: u64,
        durations: u64,
    },
    PluginIdentityDoesNotMatchScope {
        scope: PluginScope,
        plugin_present: bool,
        subject_present: bool,
    },
    SessionCounts(SessionCountError),
}

impl From<SessionCountError> for PluginHookMetricsError {
    fn from(error: SessionCountError) -> Self {
        Self::SessionCounts(error)
    }
}

impl fmt::Display for PluginHookMetricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAttempts => formatter.write_str("plugin-hook metrics row has no attempts"),
            Self::OutcomeTotalOverflow => {
                formatter.write_str("plugin-hook outcome total overflows u64")
            }
            Self::OutcomeTotalMismatch { attempts, outcomes } => write!(
                formatter,
                "plugin-hook outcome total {outcomes} does not match {attempts} attempts"
            ),
            Self::PrepareDurationTotalOverflow => {
                formatter.write_str("plugin-hook preparation duration total overflows u64")
            }
            Self::PrepareDurationTotalMismatch {
                attempts,
                durations,
            } => write!(
                formatter,
                "plugin-hook preparation duration total {durations} does not match {attempts} attempts"
            ),
            Self::ExecutionsExceedAttempts {
                attempts,
                executions,
            } => write!(
                formatter,
                "plugin-hook executions {executions} exceed {attempts} attempts"
            ),
            Self::NonExecutedAttemptsExceedErrorOutcomes {
                non_executed,
                errors,
            } => write!(
                formatter,
                "plugin-hook non-executed attempts {non_executed} exceed {errors} error outcomes"
            ),
            Self::ExecuteDurationTotalOverflow => {
                formatter.write_str("plugin-hook execution duration total overflows u64")
            }
            Self::ExecuteDurationTotalMismatch {
                executions,
                durations,
            } => write!(
                formatter,
                "plugin-hook execution duration total {durations} does not match {executions} executions"
            ),
            Self::PluginIdentityDoesNotMatchScope {
                scope,
                plugin_present,
                subject_present,
            } => write!(
                formatter,
                "plugin coordinate and subject presence do not match plugin scope {} \
                 (coordinate present: {plugin_present}, subject present: {subject_present})",
                scope.as_str()
            ),
            Self::SessionCounts(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PluginHookMetricsError {}

#[cfg(test)]
mod tests {
    use super::super::{
        IDENTIFIER_WINDOW_TEST_STATE, RowClassification, TelemetryRow,
        assert_contract_names_with_labels, classify_row, recorded_data_example_row,
        recording_observation,
    };
    use super::*;
    use crate::telemetry::{
        identity::{PluginSubject, encode_dimension_for_test},
        state::TelemetryStateV1,
    };

    fn public_extension(
        kind: ExtensionKind,
        source: PublicExtensionSource,
        name: &str,
    ) -> PublicExtensionCoordinate {
        PublicExtensionCoordinate::try_new(kind, source, name).unwrap()
    }

    fn public_plugin(source: PublicExtensionSource, name: &str) -> PublicPluginCoordinate {
        PublicPluginCoordinate::try_new(source, name).unwrap()
    }

    fn plugin_subject(coordinate: &PublicPluginCoordinate) -> PluginSubject {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);

        recording.identifier_window_scope().derive(coordinate)
    }

    #[test]
    fn plugin_scopes_round_trip_with_contract_names() {
        assert_contract_names_with_labels(
            &[
                (PluginScope::Public, "public"),
                (PluginScope::Unnamed, "unnamed"),
                (PluginScope::Overflow, "overflow"),
            ],
            PluginScope::as_str,
        );
    }

    #[test]
    fn plugin_hook_vocabulary_rejects_unknown_contract_names() {
        let scope = serde_json::from_str::<PluginScope>(r#""private""#);

        assert!(scope.is_err());
    }

    #[test]
    fn plugin_hook_outcomes_follow_error_then_blocked_precedence() {
        let cases = [
            (
                PluginHookOutcomeSignals {
                    error: false,
                    blocked: false,
                },
                PluginHookOutcome::Ok,
            ),
            (
                PluginHookOutcomeSignals {
                    error: false,
                    blocked: true,
                },
                PluginHookOutcome::Blocked,
            ),
            (
                PluginHookOutcomeSignals {
                    error: true,
                    blocked: false,
                },
                PluginHookOutcome::Error,
            ),
            (
                PluginHookOutcomeSignals {
                    error: true,
                    blocked: true,
                },
                PluginHookOutcome::Error,
            ),
        ];

        for (signals, expected) in cases {
            let outcome = PluginHookOutcome::from_signals(signals);

            assert_eq!(outcome, expected);
        }
    }

    #[test]
    fn plugin_hook_outcome_counters_round_trip_in_contract_order() {
        let counters = PluginHookOutcomeCounters {
            ok: 1,
            blocked: 2,
            error: 3,
        };
        let expected = r#"{"ok":1,"blocked":2,"error":3}"#;

        let json = serde_json::to_string(&counters).unwrap();
        let decoded = serde_json::from_str::<PluginHookOutcomeCounters>(&json).unwrap();

        assert_eq!(json, expected);
        assert_eq!(decoded, counters);
    }

    #[test]
    fn recording_each_plugin_hook_outcome_increments_only_its_counter() {
        let cases = [
            (PluginHookOutcome::Ok, r#"{"ok":1,"blocked":0,"error":0}"#),
            (
                PluginHookOutcome::Blocked,
                r#"{"ok":0,"blocked":1,"error":0}"#,
            ),
            (
                PluginHookOutcome::Error,
                r#"{"ok":0,"blocked":0,"error":1}"#,
            ),
        ];

        for (outcome, expected) in cases {
            let mut counters = PluginHookOutcomeCounters::default();

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
    fn plugin_hook_outcome_counters_reject_missing_unknown_and_invalid_fields() {
        let cases = [
            r#"{"ok":0,"blocked":0}"#,
            r#"{"ok":0,"blocked":0,"error":0,"future":0}"#,
            r#"{"ok":"none","blocked":0,"error":0}"#,
            r#"{"ok":-1,"blocked":0,"error":0}"#,
        ];

        for json in cases {
            assert!(
                serde_json::from_str::<PluginHookOutcomeCounters>(json).is_err(),
                "accepted invalid plugin-hook outcome counters {json}"
            );
        }
    }

    #[test]
    fn recording_a_plugin_hook_outcome_rejects_overflow_without_mutation() {
        let outcomes = [
            PluginHookOutcome::Ok,
            PluginHookOutcome::Blocked,
            PluginHookOutcome::Error,
        ];

        for outcome in outcomes {
            let mut counters = PluginHookOutcomeCounters {
                ok: u64::MAX,
                blocked: u64::MAX,
                error: u64::MAX,
            };
            let before = counters;

            let result = counters.checked_record(outcome);

            assert_eq!(result, Err(PluginHookOutcomeCounterOverflow { outcome }));
            assert_eq!(counters, before);
        }
    }

    #[test]
    fn plugin_hook_outcome_counter_total_is_checked_for_overflow() {
        let representable = PluginHookOutcomeCounters {
            ok: 1,
            blocked: 2,
            error: 3,
        };
        let overflowing = PluginHookOutcomeCounters {
            ok: u64::MAX,
            blocked: 1,
            error: 0,
        };

        assert_eq!(representable.checked_total(), Some(6));
        assert_eq!(overflowing.checked_total(), None);
    }

    #[test]
    fn public_plugin_coordinate_is_derived_from_a_validated_plugin() {
        let extension = public_extension(
            ExtensionKind::Plugin,
            PublicExtensionSource::SymposiumRecommendations,
            "Example-tools_2",
        );

        let coordinate = PublicPluginCoordinate::try_from(&extension).unwrap();
        let json = serde_json::to_string(&coordinate).unwrap();
        let decoded = serde_json::from_str::<PublicPluginCoordinate>(&json).unwrap();

        assert_eq!(
            json,
            r#"{"source":"symposium-recommendations","name":"Example-tools_2"}"#
        );
        assert_eq!(decoded, coordinate);
        assert_eq!(
            coordinate.source(),
            PublicExtensionSource::SymposiumRecommendations
        );
        assert_eq!(coordinate.name().as_str(), "Example-tools_2");
    }

    #[test]
    fn public_plugin_coordinate_rejects_a_skill_coordinate() {
        let skill = public_extension(
            ExtensionKind::Skill,
            PublicExtensionSource::SymposiumRecommendations,
            "example-debugging",
        );

        let result = PublicPluginCoordinate::try_from(&skill);

        assert_eq!(
            result,
            Err(NotPublicPlugin {
                found: ExtensionKind::Skill,
            })
        );
    }

    #[test]
    fn public_plugin_coordinate_rejects_invalid_or_unknown_fields() {
        let invalid_raw =
            PublicPluginCoordinate::try_new(PublicExtensionSource::CratesIo, "example.tools");
        let invalid_name = serde_json::from_str::<PublicPluginCoordinate>(
            r#"{"source":"crates-io","name":"example.tools"}"#,
        );
        let unknown_field = serde_json::from_str::<PublicPluginCoordinate>(
            r#"{"source":"crates-io","name":"example-tools","type":"plugin"}"#,
        );
        let missing_field =
            serde_json::from_str::<PublicPluginCoordinate>(r#"{"source":"crates-io"}"#);

        assert_eq!(
            invalid_raw,
            Err(PublicExtensionNameError::UnsupportedCharacter)
        );
        assert!(invalid_name.is_err());
        assert!(unknown_field.is_err());
        assert!(missing_field.is_err());
    }

    #[test]
    fn plugin_subject_dimension_uses_source_then_name() {
        let coordinate = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "example-tools",
        );
        let expected = [
            [0, 0, 0, 0, 0, 0, 0, 25].as_slice(),
            b"symposium-recommendations".as_slice(),
            [0, 0, 0, 0, 0, 0, 0, 13].as_slice(),
            b"example-tools".as_slice(),
        ]
        .concat();

        let encoded = encode_dimension_for_test(&coordinate);

        assert_eq!(encoded, expected);
    }

    #[test]
    fn plugin_subject_derivation_matches_independent_vector() {
        let coordinate = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "example-tools",
        );
        // Cross-checked with .NET's HMACSHA256 over the contract header,
        // identifier window, public source, and plugin name. The complete
        // digest is
        // 7af980f6c5e53598991b1ac981cffb14c5b48919349bcd2494961fb512f74d71.
        let expected = "plg_7af980f6c5e53598991b1ac981cffb14".parse().unwrap();

        let subject = plugin_subject(&coordinate);

        assert_eq!(subject, expected);
    }

    #[test]
    fn plugin_subject_changes_with_source_or_name() {
        let baseline = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "example-tools",
        );
        let other_source = public_plugin(PublicExtensionSource::CratesIo, "example-tools");
        let other_name = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "other-tools",
        );

        let baseline = plugin_subject(&baseline);
        let other_source = plugin_subject(&other_source);
        let other_name = plugin_subject(&other_name);

        assert_ne!(baseline, other_source);
        assert_ne!(baseline, other_name);
    }

    fn plugin_hook_metrics_value() -> serde_json::Value {
        serde_json::from_str(recorded_data_example_row("plugin_hook_metrics")).unwrap()
    }

    fn plugin_hook_metrics_error(value: serde_json::Value) -> String {
        serde_json::from_value::<PluginHookMetricsV1>(value)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn plugin_hook_metrics_example_round_trips_through_the_classifier() {
        let source = recorded_data_example_row("plugin_hook_metrics");

        let RowClassification::Supported(TelemetryRow::PluginHookMetrics(row)) =
            classify_row(source)
        else {
            panic!("documented plugin-hook metrics row was not classified as supported");
        };
        let serialized = serde_json::to_string(&row).unwrap();

        assert_eq!(serialized, source);
    }

    #[test]
    fn plugin_hook_metrics_rejects_future_versions_unknown_fields_and_missing_fields() {
        let mut future_version = plugin_hook_metrics_value();
        future_version["v"] = serde_json::Value::from(2);
        let future_json = serde_json::to_string(&future_version).unwrap();
        let mut unknown_field = plugin_hook_metrics_value();
        unknown_field["future"] = serde_json::Value::Bool(true);
        let mut missing_field = plugin_hook_metrics_value();
        missing_field.as_object_mut().unwrap().remove("executions");

        assert_eq!(classify_row(&future_json), RowClassification::UnknownSchema);
        for value in [future_version, unknown_field, missing_field] {
            assert!(serde_json::from_value::<PluginHookMetricsV1>(value).is_err());
        }
    }

    #[test]
    fn plugin_hook_metrics_rejects_zero_attempts() {
        let mut value = plugin_hook_metrics_value();
        value["attempts"] = serde_json::Value::from(0);
        let json = serde_json::to_string(&value).unwrap();

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook metrics row has no attempts"));
        assert_eq!(classify_row(&json), RowClassification::Invalid);
    }

    #[test]
    fn plugin_hook_metrics_rejects_an_outcome_total_that_differs_from_attempts() {
        let mut value = plugin_hook_metrics_value();
        value["outcomes"]["ok"] = serde_json::Value::from(498);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook outcome total 499 does not match 500 attempts"));
    }

    #[test]
    fn plugin_hook_metrics_rejects_an_overflowing_outcome_total() {
        let mut value = plugin_hook_metrics_value();
        value["outcomes"]["ok"] = serde_json::Value::from(u64::MAX);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook outcome total overflows u64"));
    }

    #[test]
    fn plugin_hook_metrics_rejects_a_prepare_total_that_differs_from_attempts() {
        let mut value = plugin_hook_metrics_value();
        value["prepare_ms"]["counts"][0] = serde_json::Value::from(399);

        let error = plugin_hook_metrics_error(value);

        assert!(
            error
                .contains("plugin-hook preparation duration total 499 does not match 500 attempts")
        );
    }

    #[test]
    fn plugin_hook_metrics_rejects_an_overflowing_prepare_total() {
        let mut value = plugin_hook_metrics_value();
        value["prepare_ms"]["counts"][0] = serde_json::Value::from(u64::MAX);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook preparation duration total overflows u64"));
    }

    #[test]
    fn plugin_hook_metrics_rejects_more_executions_than_attempts() {
        let mut value = plugin_hook_metrics_value();
        value["executions"] = serde_json::Value::from(501);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook executions 501 exceed 500 attempts"));
    }

    #[test]
    fn plugin_hook_metrics_rejects_non_executed_attempts_without_error_outcomes() {
        let mut value = plugin_hook_metrics_value();
        value["attempts"] = serde_json::Value::from(2);
        value["executions"] = serde_json::Value::from(0);
        value["outcomes"] = serde_json::json!({"ok": 2, "blocked": 0, "error": 0});
        value["prepare_ms"]["counts"] = serde_json::json!([2, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["execute_ms"]["counts"] = serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["identified_sessions_non_ok"] = serde_json::Value::from(0);
        let json = serde_json::to_string(&value).unwrap();

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook non-executed attempts 2 exceed 0 error outcomes"));
        assert_eq!(classify_row(&json), RowClassification::Invalid);
    }

    #[test]
    fn plugin_hook_metrics_accepts_one_error_for_one_non_executed_attempt() {
        let mut value = plugin_hook_metrics_value();
        value["attempts"] = serde_json::Value::from(2);
        value["executions"] = serde_json::Value::from(1);
        value["outcomes"] = serde_json::json!({"ok": 1, "blocked": 0, "error": 1});
        value["prepare_ms"]["counts"] = serde_json::json!([2, 0, 0, 0, 0, 0, 0, 0, 0]);
        value["execute_ms"]["counts"] = serde_json::json!([1, 0, 0, 0, 0, 0, 0, 0, 0]);

        let row = serde_json::from_value::<PluginHookMetricsV1>(value);

        assert!(row.is_ok());
    }

    #[test]
    fn plugin_hook_metrics_rejects_an_execute_total_that_differs_from_executions() {
        let mut value = plugin_hook_metrics_value();
        value["execute_ms"]["counts"][7] = serde_json::Value::from(1);

        let error = plugin_hook_metrics_error(value);

        assert!(
            error
                .contains("plugin-hook execution duration total 501 does not match 500 executions")
        );
    }

    #[test]
    fn plugin_hook_metrics_rejects_an_overflowing_execute_total() {
        let mut value = plugin_hook_metrics_value();
        value["execute_ms"]["counts"][0] = serde_json::Value::from(u64::MAX);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("plugin-hook execution duration total overflows u64"));
    }

    #[test]
    fn public_plugin_hook_metrics_require_a_coordinate_and_subject() {
        for fields_to_remove in [
            &["plugin"][..],
            &["plugin_subject"][..],
            &["plugin", "plugin_subject"][..],
        ] {
            let mut value = plugin_hook_metrics_value();
            for field in fields_to_remove {
                value.as_object_mut().unwrap().remove(*field);
            }

            let error = plugin_hook_metrics_error(value);

            assert!(error.contains("do not match plugin scope public"));
        }
    }

    #[test]
    fn unnamed_and_overflow_plugin_hook_metrics_omit_public_identity() {
        for scope in [PluginScope::Unnamed, PluginScope::Overflow] {
            let mut value = plugin_hook_metrics_value();
            value["plugin_scope"] = serde_json::to_value(scope).unwrap();
            value.as_object_mut().unwrap().remove("plugin");
            value.as_object_mut().unwrap().remove("plugin_subject");

            let row = serde_json::from_value::<PluginHookMetricsV1>(value).unwrap();
            let serialized = serde_json::to_value(row).unwrap();

            assert!(serialized.get("plugin").is_none());
            assert!(serialized.get("plugin_subject").is_none());
        }
    }

    #[test]
    fn unnamed_and_overflow_plugin_hook_metrics_reject_public_identity() {
        for scope in [PluginScope::Unnamed, PluginScope::Overflow] {
            for field_to_keep in ["plugin", "plugin_subject"] {
                let mut value = plugin_hook_metrics_value();
                value["plugin_scope"] = serde_json::to_value(scope).unwrap();
                for field in ["plugin", "plugin_subject"] {
                    if field != field_to_keep {
                        value.as_object_mut().unwrap().remove(field);
                    }
                }

                let error = plugin_hook_metrics_error(value);

                assert!(error.contains(&format!("do not match plugin scope {}", scope.as_str())));
            }
        }
    }

    #[test]
    fn plugin_hook_metrics_apply_the_shared_session_count_rules() {
        let mut value = plugin_hook_metrics_value();
        value["identified_sessions"] = serde_json::Value::from(501);

        let error = plugin_hook_metrics_error(value);

        assert!(error.contains("identified sessions 501 exceed the version 1 limit 256"));
    }
}
