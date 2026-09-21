//! Plugin-hook public identity, bounded buckets, and aggregate keys.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::super::{
    agent::HookAgent,
    extension::{
        ExtensionKind, PublicExtensionCoordinate, PublicExtensionName, PublicExtensionNameError,
        PublicExtensionSource,
    },
    hook::{HookMetricsKey, HookSurface},
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
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unnamed => "unnamed",
            Self::Overflow => "overflow",
        }
    }
}

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
    pub(super) fn into_row_identity(
        self,
        recording: &BoundRecordingObservation<'_>,
    ) -> PluginHookRowIdentity {
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
pub(super) struct PluginHookRowIdentity {
    pub(super) scope: PluginScope,
    pub(super) plugin: Option<PublicPluginCoordinate>,
    pub(super) plugin_subject: Option<PluginSubject>,
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

/// Plugin identity observed before private-state admission.
///
/// Overflow is deliberately absent. Only the private aggregate store may
/// assign a public observation to the bounded overflow row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::telemetry) enum PluginHookAttribution {
    Public(PublicPluginCoordinate),
    Unnamed,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::telemetry::{
        identity::{PluginSubject, encode_dimension_for_test},
        schema::{
            IDENTIFIER_WINDOW_TEST_STATE, UtcSecond, assert_contract_names_with_labels,
            recording_observation,
        },
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
}
