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
    hook::{HookMetricsKey, HookSurface},
    macros::strict_versioned_row,
    metrics::{LatencyHistogram, SessionCountError, SessionCountInput, validate_session_counts},
};
use crate::telemetry::{
    identity::{DimensionWriter, IdentityDimension, PluginDomain, PluginSubject},
    state::BoundRecordingObservation,
};

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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// Plugin identity bucket selected for one aggregate observation.
///
/// Public buckets carry a validated coordinate. Unnamed and overflow buckets
/// cannot carry public identity, so the row fields derived from this value
/// cannot represent a mismatched scope, coordinate, and subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::telemetry) enum PluginBucket {
    Public(PublicPluginCoordinate),
    Unnamed,
    Overflow,
}

impl PluginBucket {
    /// Derive the wire identity for this bucket in the active identifier
    /// window.
    fn into_row_identity(self, recording: &BoundRecordingObservation<'_>) -> PluginHookRowIdentity {
        match self {
            Self::Public(plugin) => {
                let plugin_subject = recording.identifier_window_scope().derive(&plugin);
                PluginHookRowIdentity {
                    scope: PluginScope::Public,
                    plugin: Some(plugin),
                    plugin_subject: Some(plugin_subject),
                }
            }
            Self::Unnamed => PluginHookRowIdentity {
                scope: PluginScope::Unnamed,
                plugin: None,
                plugin_subject: None,
            },
            Self::Overflow => PluginHookRowIdentity {
                scope: PluginScope::Overflow,
                plugin: None,
                plugin_subject: None,
            },
        }
    }
}

/// Wire identity fields derived from one [`PluginBucket`].
struct PluginHookRowIdentity {
    scope: PluginScope,
    plugin: Option<PublicPluginCoordinate>,
    plugin_subject: Option<PluginSubject>,
}

/// Stable lookup key for one daily plugin-hook aggregate.
///
/// The hook key binds the day, agent, hook surface, and identifier epoch. The
/// bucket then separates public plugins from the unnamed and overflow rows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::telemetry) struct PluginHookMetricsKey {
    hook: HookMetricsKey,
    bucket: PluginBucket,
}

impl PluginHookMetricsKey {
    /// Select one plugin-hook aggregate from a bound recording context.
    #[must_use]
    pub(in crate::telemetry) fn new(
        recording: &BoundRecordingObservation<'_>,
        agent: HookAgent,
        hook: HookSurface,
        bucket: PluginBucket,
    ) -> Self {
        Self {
            hook: HookMetricsKey::new(recording, agent, hook),
            bucket,
        }
    }
}

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
    use chrono::{TimeZone, Utc};

    use super::super::{
        IDENTIFIER_WINDOW_TEST_STATE, RowClassification, TelemetryRow, UtcSecond,
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

    #[test]
    fn public_plugin_bucket_supplies_the_complete_wire_identity() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);
        let plugin = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "example-tools",
        );
        let expected_subject = plugin_subject(&plugin);

        let identity = PluginBucket::Public(plugin.clone()).into_row_identity(&recording);

        assert_eq!(identity.scope, PluginScope::Public);
        assert_eq!(identity.plugin, Some(plugin));
        assert_eq!(identity.plugin_subject, Some(expected_subject));
    }

    #[test]
    fn unnamed_plugin_buckets_omit_public_wire_identity() {
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let recording = recording_observation(&mut state);

        for (bucket, expected_scope) in [
            (PluginBucket::Unnamed, PluginScope::Unnamed),
            (PluginBucket::Overflow, PluginScope::Overflow),
        ] {
            let identity = bucket.into_row_identity(&recording);

            assert_eq!(identity.scope, expected_scope);
            assert_eq!(identity.plugin, None);
            assert_eq!(identity.plugin_subject, None);
        }
    }

    #[test]
    fn plugin_hook_key_is_stable_and_separates_plugins_hooks_days_and_epochs() {
        let plugin = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "example-tools",
        );
        let other_plugin = public_plugin(
            PublicExtensionSource::SymposiumRecommendations,
            "other-tools",
        );
        let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        let (baseline, another_plugin, another_hook, unnamed, baseline_day) = {
            let recording = recording_observation(&mut state);
            (
                PluginHookMetricsKey::new(
                    &recording,
                    HookAgent::Claude,
                    HookSurface::PreToolUse,
                    PluginBucket::Public(plugin.clone()),
                ),
                PluginHookMetricsKey::new(
                    &recording,
                    HookAgent::Claude,
                    HookSurface::PreToolUse,
                    PluginBucket::Public(other_plugin),
                ),
                PluginHookMetricsKey::new(
                    &recording,
                    HookAgent::Claude,
                    HookSurface::PostToolUse,
                    PluginBucket::Public(plugin.clone()),
                ),
                PluginHookMetricsKey::new(
                    &recording,
                    HookAgent::Claude,
                    HookSurface::PreToolUse,
                    PluginBucket::Unnamed,
                ),
                recording.day(),
            )
        };
        let same_day_at =
            UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 8, 3, 11, 2, 11).unwrap());
        let same_day = state.observe_recording(same_day_at).unwrap();
        let same_day = state.bind_recording_observation(same_day).unwrap();
        let same_selection = PluginHookMetricsKey::new(
            &same_day,
            HookAgent::Claude,
            HookSurface::PreToolUse,
            PluginBucket::Public(plugin.clone()),
        );
        let later_at =
            UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 8, 4, 10, 2, 11).unwrap());
        let later = state.observe_recording(later_at).unwrap();
        let later = state.bind_recording_observation(later).unwrap();
        let another_day = PluginHookMetricsKey::new(
            &later,
            HookAgent::Claude,
            HookSurface::PreToolUse,
            PluginBucket::Public(plugin.clone()),
        );

        let mut reset_state: TelemetryStateV1 =
            toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
        reset_state.reset_identifiers(baseline_day).unwrap();
        let reset_recording = recording_observation(&mut reset_state);
        let another_epoch = PluginHookMetricsKey::new(
            &reset_recording,
            HookAgent::Claude,
            HookSurface::PreToolUse,
            PluginBucket::Unnamed,
        );

        assert_eq!(baseline, same_selection);
        assert_ne!(baseline, another_plugin);
        assert_ne!(baseline, another_hook);
        assert_ne!(baseline, another_day);
        assert_ne!(unnamed, another_epoch);
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

mod update {
    //! Atomic write-side updates for plugin-hook aggregate rows.

    use std::{fmt, time::Duration};

    use super::{
        PluginBucket, PluginHookMetricsKey, PluginHookMetricsV1, PluginHookOutcome,
        PluginHookOutcomeCounterOverflow, PluginHookOutcomeCounters, PluginHookOutcomeSignals,
    };
    use crate::telemetry::{
        schema::{
            SchemaVersion, SymposiumVersion, UtcDay,
            agent::{HookAgent, VendorSessionId, derive_session_id},
            hook::HookSurface,
            metrics::{LatencyHistogram, LatencyHistogramError},
        },
        state::{
            BoundRecordingObservation, HookSessionCountSnapshot, HookSessionCountUpdateError,
            SelectedPluginHookAggregate,
        },
    };

    /// Whether one plugin-hook attempt started its child process.
    ///
    /// A child that never starts always contributes an `error` outcome. An
    /// executed child carries its final outcome and both durations, so impossible
    /// combinations such as an unexecuted `ok` attempt cannot reach the row.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::telemetry) enum PluginHookAttempt {
        NotExecuted {
            prepare_duration: Duration,
        },
        Executed {
            signals: PluginHookOutcomeSignals,
            prepare_duration: Duration,
            execute_duration: Duration,
        },
    }

    impl PluginHookAttempt {
        #[must_use]
        pub(super) const fn outcome(self) -> PluginHookOutcome {
            match self {
                Self::NotExecuted { .. } => PluginHookOutcome::Error,
                Self::Executed { signals, .. } => PluginHookOutcome::from_signals(signals),
            }
        }

        #[must_use]
        pub(super) const fn prepare_duration(self) -> Duration {
            match self {
                Self::NotExecuted { prepare_duration }
                | Self::Executed {
                    prepare_duration, ..
                } => prepare_duration,
            }
        }

        #[must_use]
        pub(super) const fn execute_duration(self) -> Option<Duration> {
            match self {
                Self::NotExecuted { .. } => None,
                Self::Executed {
                    execute_duration, ..
                } => Some(execute_duration),
            }
        }
    }

    /// One completed plugin-hook attempt added to an aggregate row.
    ///
    /// Named fields keep the aggregate target, process lifecycle, and optional
    /// vendor session identifier together at the write boundary.
    // Intentionally omit `Debug`: this value borrows a raw vendor session id.
    #[derive(Clone)]
    pub(in crate::telemetry) struct PluginHookMetricObservation<'a> {
        pub(in crate::telemetry) agent: HookAgent,
        pub(in crate::telemetry) hook: HookSurface,
        pub(in crate::telemetry) bucket: PluginBucket,
        pub(in crate::telemetry) attempt: PluginHookAttempt,
        pub(in crate::telemetry) vendor_session_id: Option<&'a VendorSessionId>,
    }

    impl PluginHookMetricObservation<'_> {
        #[must_use]
        fn key(&self, recording: &BoundRecordingObservation<'_>) -> PluginHookMetricsKey {
            PluginHookMetricsKey::new(recording, self.agent, self.hook, self.bucket.clone())
        }
    }

    impl PluginHookMetricsV1 {
        /// Start an aggregate row with its first completed plugin-hook attempt.
        ///
        /// The selected private-state entry supplies the stable row identifier and
        /// session tracker together. The row day and public identity come from the
        /// same bound recording and plugin bucket used to select that entry.
        ///
        /// # Errors
        ///
        /// Returns [`PluginHookMetricsUpdateError`] when any counter cannot
        /// represent the attempt or the supplied context selects another row.
        #[must_use = "a failed plugin-hook metric update must be dropped"]
        pub(in crate::telemetry) fn new(
            recording: &BoundRecordingObservation<'_>,
            observation: PluginHookMetricObservation<'_>,
            selected: SelectedPluginHookAggregate<'_>,
        ) -> Result<Self, PluginHookMetricsUpdateError> {
            let identity = observation.bucket.clone().into_row_identity(recording);
            let mut row = Self {
                version: SchemaVersion::V1,
                kind: Self::KIND,
                event_id: selected.event_id(),
                day: recording.day(),
                symposium: SymposiumVersion::current(),
                agent: observation.agent,
                hook: observation.hook,
                plugin_scope: identity.scope,
                plugin: identity.plugin,
                attempts: 0,
                executions: 0,
                outcomes: PluginHookOutcomeCounters::default(),
                prepare_ms: LatencyHistogram::default(),
                execute_ms: LatencyHistogram::default(),
                identified_sessions: None,
                identified_sessions_non_ok: None,
                session_counts_complete: false,
                plugin_subject: identity.plugin_subject,
            };

            row.checked_record(recording, observation, selected)?;
            Ok(row)
        }

        /// Add one plugin-hook attempt without partially changing its row or
        /// private session tracker.
        ///
        /// Every row counter is updated on a clone first. The tracker update is the
        /// final fallible operation and remains unchanged on failure; publishing
        /// its snapshot and replacing the row are then infallible. Passing the
        /// complete selected entry keeps its event identifier and tracker paired.
        ///
        /// # Errors
        ///
        /// Returns [`PluginHookMetricsUpdateError`] when the recording or private
        /// state selects another aggregate, or when a counter would overflow. Both
        /// the row and selected private state remain unchanged on failure.
        #[must_use = "a failed plugin-hook metric update must be dropped"]
        pub(in crate::telemetry) fn checked_record(
            &mut self,
            recording: &BoundRecordingObservation<'_>,
            observation: PluginHookMetricObservation<'_>,
            mut selected: SelectedPluginHookAggregate<'_>,
        ) -> Result<(), PluginHookMetricsUpdateError> {
            self.ensure_selected_by(recording, &observation, &selected)?;

            let outcome = observation.attempt.outcome();
            let mut next = self.clone();
            next.attempts = next
                .attempts
                .checked_add(1)
                .ok_or(PluginHookMetricsUpdateError::AttemptCountOverflow)?;
            next.outcomes.checked_record(outcome)?;
            next.prepare_ms
                .checked_record(observation.attempt.prepare_duration())
                .map_err(PluginHookMetricsUpdateError::PrepareDuration)?;

            if let Some(duration) = observation.attempt.execute_duration() {
                next.executions = next
                    .executions
                    .checked_add(1)
                    .ok_or(PluginHookMetricsUpdateError::ExecutionCountOverflow)?;
                next.execute_ms
                    .checked_record(duration)
                    .map_err(PluginHookMetricsUpdateError::ExecuteDuration)?;
            }

            let session_id = derive_session_id(
                recording.identifier_window_scope(),
                observation.agent,
                observation.vendor_session_id,
            );
            // `attempts + 1` succeeded above, so the tracker's matching
            // contribution increment cannot overflow here.
            let session_counts = selected.session_counts();
            session_counts.checked_record(self.attempts, session_id, outcome)?;
            next.apply_session_counts(session_counts.snapshot());

            *self = next;
            Ok(())
        }

        fn ensure_selected_by(
            &self,
            recording: &BoundRecordingObservation<'_>,
            observation: &PluginHookMetricObservation<'_>,
            selected: &SelectedPluginHookAggregate<'_>,
        ) -> Result<(), PluginHookMetricsUpdateError> {
            if self.day != recording.day() {
                return Err(PluginHookMetricsUpdateError::DayChanged {
                    row_day: self.day,
                    observation_day: recording.day(),
                });
            }
            if self.agent != observation.agent || self.hook != observation.hook {
                return Err(PluginHookMetricsUpdateError::TargetChanged);
            }

            let identity = observation.bucket.clone().into_row_identity(recording);
            if self.plugin_scope != identity.scope
                || self.plugin != identity.plugin
                || self.plugin_subject != identity.plugin_subject
            {
                return Err(PluginHookMetricsUpdateError::PluginIdentityChanged);
            }

            let key = observation.key(recording);
            if selected.key() != &key {
                return Err(PluginHookMetricsUpdateError::PrivateStateChanged);
            }
            if self.event_id != selected.event_id() {
                return Err(PluginHookMetricsUpdateError::RowIdentifierChanged);
            }

            Ok(())
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

    /// A plugin-hook aggregate update that cannot be represented safely.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::telemetry) enum PluginHookMetricsUpdateError {
        DayChanged {
            row_day: UtcDay,
            observation_day: UtcDay,
        },
        TargetChanged,
        PluginIdentityChanged,
        PrivateStateChanged,
        RowIdentifierChanged,
        AttemptCountOverflow,
        ExecutionCountOverflow,
        OutcomeCountOverflow {
            outcome: PluginHookOutcome,
        },
        PrepareDuration(LatencyHistogramError),
        ExecuteDuration(LatencyHistogramError),
        SessionCounts(HookSessionCountUpdateError),
    }

    impl From<PluginHookOutcomeCounterOverflow> for PluginHookMetricsUpdateError {
        fn from(error: PluginHookOutcomeCounterOverflow) -> Self {
            Self::OutcomeCountOverflow {
                outcome: error.outcome,
            }
        }
    }

    impl From<HookSessionCountUpdateError> for PluginHookMetricsUpdateError {
        fn from(error: HookSessionCountUpdateError) -> Self {
            Self::SessionCounts(error)
        }
    }

    impl fmt::Display for PluginHookMetricsUpdateError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::DayChanged {
                    row_day,
                    observation_day,
                } => write!(
                    formatter,
                    "plugin-hook metrics row belongs to {row_day}, not observed day {observation_day}"
                ),
                Self::TargetChanged => {
                    formatter.write_str("plugin-hook aggregate target changed during update")
                }
                Self::PluginIdentityChanged => {
                    formatter.write_str("plugin-hook aggregate identity changed during update")
                }
                Self::PrivateStateChanged => {
                    formatter.write_str("plugin-hook private state belongs to another aggregate")
                }
                Self::RowIdentifierChanged => formatter
                    .write_str("plugin-hook private state belongs to another aggregate row"),
                Self::AttemptCountOverflow => {
                    formatter.write_str("plugin-hook attempt count overflows u64")
                }
                Self::ExecutionCountOverflow => {
                    formatter.write_str("plugin-hook execution count overflows u64")
                }
                Self::OutcomeCountOverflow { outcome } => write!(
                    formatter,
                    "{} plugin-hook outcome counter overflows u64",
                    outcome.counter_name()
                ),
                Self::PrepareDuration(error) => error.fmt(formatter),
                Self::ExecuteDuration(error) => error.fmt(formatter),
                Self::SessionCounts(error) => error.fmt(formatter),
            }
        }
    }

    impl std::error::Error for PluginHookMetricsUpdateError {}

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use super::super::{PluginHookOutcomeSignals, PluginScope, PublicPluginCoordinate};
        use super::*;
        use crate::telemetry::{
            schema::{
                IDENTIFIER_WINDOW_TEST_STATE, RowClassification, TelemetryRow, UtcSecond,
                classify_row, extension::PublicExtensionSource, recording_observation,
            },
            state::{PluginHookAggregateState, TelemetryStateV1},
        };
        use chrono::{TimeZone as _, Utc};

        fn public_plugin(source: PublicExtensionSource, name: &str) -> PublicPluginCoordinate {
            PublicPluginCoordinate::try_new(source, name).unwrap()
        }

        fn successful_attempt() -> PluginHookAttempt {
            PluginHookAttempt::Executed {
                signals: PluginHookOutcomeSignals {
                    error: false,
                    blocked: false,
                },
                prepare_duration: Duration::from_millis(1),
                execute_duration: Duration::from_millis(1),
            }
        }

        fn metric_observation<'a>(
            bucket: PluginBucket,
            attempt: PluginHookAttempt,
            vendor_session_id: Option<&'a VendorSessionId>,
        ) -> PluginHookMetricObservation<'a> {
            PluginHookMetricObservation {
                agent: HookAgent::Claude,
                hook: HookSurface::PreToolUse,
                bucket,
                attempt,
                vendor_session_id,
            }
        }

        fn private_state(
            recording: &BoundRecordingObservation<'_>,
            observation: &PluginHookMetricObservation<'_>,
        ) -> PluginHookAggregateState {
            PluginHookAggregateState::for_test(observation.key(recording))
        }

        fn initialized_aggregate(
            recording: &BoundRecordingObservation<'_>,
            bucket: PluginBucket,
        ) -> (
            PluginHookMetricsV1,
            PluginHookAggregateState,
            PluginHookMetricsKey,
        ) {
            let observation = metric_observation(bucket, successful_attempt(), None);
            let key = observation.key(recording);
            let mut private = private_state(recording, &observation);
            let row =
                PluginHookMetricsV1::new(recording, observation, private.select(&key).unwrap())
                    .unwrap();

            (row, private, key)
        }

        fn assert_update_rejected_without_mutation(
            row: &mut PluginHookMetricsV1,
            recording: &BoundRecordingObservation<'_>,
            observation: PluginHookMetricObservation<'_>,
            private: &mut PluginHookAggregateState,
            selected_key: &PluginHookMetricsKey,
            expected: PluginHookMetricsUpdateError,
        ) {
            let row_before = row.clone();
            let private_before = private.clone();

            let result = row.checked_record(
                recording,
                observation,
                private.select(selected_key).unwrap(),
            );

            assert_eq!(result, Err(expected));
            assert_eq!(*row, row_before);
            assert_eq!(*private, private_before);
        }

        #[test]
        fn executed_attempt_applies_outcome_signal_precedence() {
            let attempt = PluginHookAttempt::Executed {
                signals: PluginHookOutcomeSignals {
                    error: true,
                    blocked: true,
                },
                prepare_duration: Duration::from_millis(1),
                execute_duration: Duration::from_millis(1),
            };

            assert_eq!(attempt.outcome(), PluginHookOutcome::Error);
        }

        #[test]
        fn first_unexecuted_plugin_attempt_populates_an_error_aggregate() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
            let plugin = public_plugin(
                PublicExtensionSource::SymposiumRecommendations,
                "example-tools",
            );
            let observation = metric_observation(
                PluginBucket::Public(plugin.clone()),
                PluginHookAttempt::NotExecuted {
                    prepare_duration: Duration::from_millis(7),
                },
                Some(&vendor_session_id),
            );
            let key = observation.key(&recording);
            let mut private = private_state(&recording, &observation);
            let event_id = private.select(&key).unwrap().event_id();

            let row =
                PluginHookMetricsV1::new(&recording, observation, private.select(&key).unwrap())
                    .unwrap();
            let json = serde_json::to_string(&row).unwrap();
            let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();

            assert_eq!(row.event_id, event_id);
            assert_eq!(row.day, recording.day());
            assert_eq!(row.plugin_scope, PluginScope::Public);
            assert_eq!(row.plugin, Some(plugin));
            assert_eq!(row.attempts, 1);
            assert_eq!(row.executions, 0);
            assert_eq!(
                value["outcomes"],
                serde_json::json!({
                    "ok": 0,
                    "blocked": 0,
                    "error": 1,
                })
            );
            assert_eq!(
                value["prepare_ms"]["counts"],
                serde_json::json!([0, 1, 0, 0, 0, 0, 0, 0, 0])
            );
            assert_eq!(
                value["execute_ms"]["counts"],
                serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0])
            );
            assert_eq!(row.identified_sessions, Some(1));
            assert_eq!(row.identified_sessions_non_ok, Some(1));
            assert!(row.session_counts_complete);
            assert!(matches!(
                classify_row(&json),
                RowClassification::Supported(TelemetryRow::PluginHookMetrics(_))
            ));
        }

        #[test]
        fn executed_plugin_attempts_accumulate_every_metric() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let first_session = VendorSessionId::new("vendor-session-123".to_owned());
            let second_session = VendorSessionId::new("vendor-session-456".to_owned());
            let bucket = PluginBucket::Public(public_plugin(
                PublicExtensionSource::SymposiumRecommendations,
                "example-tools",
            ));
            let first = metric_observation(
                bucket.clone(),
                PluginHookAttempt::Executed {
                    signals: PluginHookOutcomeSignals {
                        error: false,
                        blocked: false,
                    },
                    prepare_duration: Duration::from_millis(5),
                    execute_duration: Duration::from_millis(25),
                },
                Some(&first_session),
            );
            let key = first.key(&recording);
            let mut private = private_state(&recording, &first);
            let mut row =
                PluginHookMetricsV1::new(&recording, first, private.select(&key).unwrap()).unwrap();
            let event_id = row.event_id;
            let second = metric_observation(
                bucket,
                PluginHookAttempt::Executed {
                    signals: PluginHookOutcomeSignals {
                        error: false,
                        blocked: true,
                    },
                    prepare_duration: Duration::from_millis(1_001),
                    execute_duration: Duration::from_millis(6),
                },
                Some(&second_session),
            );

            row.checked_record(&recording, second, private.select(&key).unwrap())
                .unwrap();
            let json = serde_json::to_string(&row).unwrap();
            let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();

            assert_eq!(row.event_id, event_id);
            assert_eq!(row.attempts, 2);
            assert_eq!(row.executions, 2);
            assert_eq!(
                value["outcomes"],
                serde_json::json!({
                    "ok": 1,
                    "blocked": 1,
                    "error": 0,
                })
            );
            assert_eq!(
                value["prepare_ms"]["counts"],
                serde_json::json!([1, 0, 0, 0, 0, 0, 0, 0, 1])
            );
            assert_eq!(
                value["execute_ms"]["counts"],
                serde_json::json!([0, 1, 1, 0, 0, 0, 0, 0, 0])
            );
            assert_eq!(row.identified_sessions, Some(2));
            assert_eq!(row.identified_sessions_non_ok, Some(1));
            assert!(row.session_counts_complete);
            assert!(matches!(
                classify_row(&json),
                RowClassification::Supported(TelemetryRow::PluginHookMetrics(_))
            ));
        }

        #[test]
        fn missing_session_id_makes_plugin_hook_counts_incomplete() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let observation = metric_observation(
                PluginBucket::Unnamed,
                PluginHookAttempt::Executed {
                    signals: PluginHookOutcomeSignals {
                        error: false,
                        blocked: false,
                    },
                    prepare_duration: Duration::from_millis(1),
                    execute_duration: Duration::from_millis(1),
                },
                None,
            );
            let key = observation.key(&recording);
            let mut private = private_state(&recording, &observation);

            let row =
                PluginHookMetricsV1::new(&recording, observation, private.select(&key).unwrap())
                    .unwrap();

            assert!(!row.session_counts_complete);
            assert_eq!(row.identified_sessions, None);
            assert_eq!(row.identified_sessions_non_ok, None);
        }

        #[test]
        fn another_plugin_identity_is_rejected_without_mutation() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let first_plugin = PluginBucket::Public(public_plugin(
                PublicExtensionSource::SymposiumRecommendations,
                "example-tools",
            ));
            let second_plugin = PluginBucket::Public(public_plugin(
                PublicExtensionSource::SymposiumRecommendations,
                "another-tools",
            ));
            let (mut row, mut private, key) = initialized_aggregate(&recording, first_plugin);
            let observation = metric_observation(second_plugin, successful_attempt(), None);

            assert_update_rejected_without_mutation(
                &mut row,
                &recording,
                observation,
                &mut private,
                &key,
                PluginHookMetricsUpdateError::PluginIdentityChanged,
            );
        }

        #[test]
        fn another_private_state_key_is_rejected_without_mutation() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let bucket = PluginBucket::Unnamed;
            let (mut row, _, _) = initialized_aggregate(&recording, bucket.clone());
            let observation = metric_observation(bucket.clone(), successful_attempt(), None);
            let mut other_observation = metric_observation(bucket, successful_attempt(), None);
            other_observation.hook = HookSurface::PostToolUse;
            let other_key = other_observation.key(&recording);
            let mut other_private = PluginHookAggregateState::for_test(other_key.clone());

            assert_update_rejected_without_mutation(
                &mut row,
                &recording,
                observation,
                &mut other_private,
                &other_key,
                PluginHookMetricsUpdateError::PrivateStateChanged,
            );
        }

        #[test]
        fn another_day_is_rejected_without_mutation() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let (mut row, mut private, key) = {
                let recording = recording_observation(&mut state);
                initialized_aggregate(&recording, PluginBucket::Unnamed)
            };
            let completed_at =
                UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 8, 4, 10, 2, 11).unwrap());
            let observation = state.observe_recording(completed_at).unwrap();
            let recording = state.bind_recording_observation(observation).unwrap();
            let observation = metric_observation(PluginBucket::Unnamed, successful_attempt(), None);
            let row_day = row.day;

            assert_update_rejected_without_mutation(
                &mut row,
                &recording,
                observation,
                &mut private,
                &key,
                PluginHookMetricsUpdateError::DayChanged {
                    row_day,
                    observation_day: recording.day(),
                },
            );
        }

        #[test]
        fn another_hook_target_is_rejected_without_mutation() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let (mut row, mut private, key) =
                initialized_aggregate(&recording, PluginBucket::Unnamed);
            let mut observation =
                metric_observation(PluginBucket::Unnamed, successful_attempt(), None);
            observation.hook = HookSurface::PostToolUse;

            assert_update_rejected_without_mutation(
                &mut row,
                &recording,
                observation,
                &mut private,
                &key,
                PluginHookMetricsUpdateError::TargetChanged,
            );
        }

        #[test]
        fn reset_epoch_uses_event_id_to_reject_an_old_unnamed_row() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let mut row = {
                let recording = recording_observation(&mut state);
                let observation = metric_observation(
                    PluginBucket::Unnamed,
                    PluginHookAttempt::Executed {
                        signals: PluginHookOutcomeSignals {
                            error: false,
                            blocked: false,
                        },
                        prepare_duration: Duration::from_millis(1),
                        execute_duration: Duration::from_millis(1),
                    },
                    None,
                );
                let key = observation.key(&recording);
                let mut private = private_state(&recording, &observation);

                PluginHookMetricsV1::new(&recording, observation, private.select(&key).unwrap())
                    .unwrap()
            };
            state.reset_identifiers(row.day).unwrap();
            let recording = recording_observation(&mut state);
            let observation = metric_observation(
                PluginBucket::Unnamed,
                PluginHookAttempt::Executed {
                    signals: PluginHookOutcomeSignals {
                        error: false,
                        blocked: false,
                    },
                    prepare_duration: Duration::from_millis(1),
                    execute_duration: Duration::from_millis(1),
                },
                None,
            );
            let key = observation.key(&recording);
            let mut private = private_state(&recording, &observation);
            let row_before = row.clone();
            let private_before = private.clone();

            let result = row.checked_record(&recording, observation, private.select(&key).unwrap());

            assert_eq!(
                result,
                Err(PluginHookMetricsUpdateError::RowIdentifierChanged)
            );
            assert_eq!(row, row_before);
            assert_eq!(private, private_before);
        }

        #[test]
        fn failed_prepare_histogram_update_preserves_row_and_private_state() {
            let mut state: TelemetryStateV1 = toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap();
            let recording = recording_observation(&mut state);
            let vendor_session_id = VendorSessionId::new("vendor-session-123".to_owned());
            let bucket = PluginBucket::Unnamed;
            let first = metric_observation(
                bucket.clone(),
                PluginHookAttempt::Executed {
                    signals: PluginHookOutcomeSignals {
                        error: false,
                        blocked: false,
                    },
                    prepare_duration: Duration::from_millis(1),
                    execute_duration: Duration::from_millis(1),
                },
                Some(&vendor_session_id),
            );
            let key = first.key(&recording);
            let mut private = private_state(&recording, &first);
            let mut row =
                PluginHookMetricsV1::new(&recording, first, private.select(&key).unwrap()).unwrap();
            row.prepare_ms = serde_json::from_value(serde_json::json!({
                "bounds": [5, 10, 25, 50, 100, 250, 500, 1000],
                "counts": [u64::MAX, 0, 0, 0, 0, 0, 0, 0, 0],
            }))
            .unwrap();
            let row_before = row.clone();
            let private_before = private.clone();
            let second = metric_observation(
                bucket,
                PluginHookAttempt::NotExecuted {
                    prepare_duration: Duration::from_millis(1),
                },
                Some(&vendor_session_id),
            );

            let result = row.checked_record(&recording, second, private.select(&key).unwrap());

            assert_eq!(
                result,
                Err(PluginHookMetricsUpdateError::PrepareDuration(
                    LatencyHistogramError::BucketCountOverflow { bucket: 0 }
                ))
            );
            assert_eq!(row, row_before);
            assert_eq!(private, private_before);
        }
    }
}
