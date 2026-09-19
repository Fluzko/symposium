//! In-memory representation of private telemetry state.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the state schema is built before persistence uses it."
    )
)]

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::{
    identity::{IdentityKey, state_key_hex},
    schema::{CohortDay, UtcDay},
};

/// Exclusive length of an identifier window in UTC-day positions.
///
/// Offsets 0 through 29 remain in the window; unlike the cohort's inclusive
/// [`CohortDay::D30`] bound, offset 30 starts a new window.
const IDENTIFIER_WINDOW_DAYS: i64 = 30;

/// The initial schema version of `telemetry-state.toml`.
///
/// Exact rather than permissive, unlike a row's `SchemaVersion`: a row written by
/// a newer binary is classified as an unknown schema and skipped, but private
/// single-writer state must never be half-understood.
#[derive(Clone, Copy)]
struct StateVersion;

impl Serialize for StateVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(1)
    }
}

impl<'de> Deserialize<'de> for StateVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u64::deserialize(deserializer)?;
        if version != 1 {
            return Err(D::Error::custom(format_args!(
                "expected telemetry state version 1, found {version}"
            )));
        }

        Ok(Self)
    }
}

/// Version 1 of the complete private telemetry state file.
///
/// A field may be absent only when absence represents a real lifecycle state,
/// in which case its type records that explicitly. Once this version ships,
/// adding a required field needs a migration or a new state version; a default
/// must not silently turn malformed state into valid state.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct TelemetryStateV1 {
    version: StateVersion,
    identity: IdentityState,
}

impl TelemetryStateV1 {
    /// Create private state anchored to the day recording first needs identity.
    ///
    /// The return cohort remains absent until a session is observed.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot generate a secret key.
    fn new(identifier_window_anchor: UtcDay) -> Result<Self, getrandom::Error> {
        let key = IdentityKey::generate()?;
        Ok(Self::with_key(identifier_window_anchor, key))
    }

    /// Construct state from identity material that has already been generated.
    #[must_use]
    fn with_key(identifier_window_anchor: UtcDay, key: IdentityKey) -> Self {
        Self {
            version: StateVersion,
            identity: IdentityState {
                key,
                identifier_window_anchor,
                return_cohort_anchor: None,
            },
        }
    }

    /// Rotate future identifiers and begin a new identifier window.
    ///
    /// `identifier_window_anchor` must be the later of the current UTC day and
    /// the durable latest-opened-day high-water mark. Unlike a stale session
    /// observation, an explicit reset is clamped to that high-water mark rather
    /// than dropped. The next observed session starts a new return cohort at
    /// D0.
    ///
    /// The selected anchor may precede the stored window anchor after a clock
    /// rollback. That is intentional: rotating the key severs the previous
    /// identity scope, so reset does not compare the new anchor with the old
    /// one.
    ///
    /// The storage-level reset must preserve the durable high-water mark and
    /// clear pending keyed session-count sets once those sibling state sections
    /// are added.
    ///
    /// Key generation completes before any state changes, so a failure leaves
    /// the existing key and anchors intact.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot generate a secret key.
    fn reset_identifiers(
        &mut self,
        identifier_window_anchor: UtcDay,
    ) -> Result<(), getrandom::Error> {
        self.reset_identifiers_with(identifier_window_anchor, getrandom::fill)
    }

    /// Reset identifiers using a caller-provided source of key bytes.
    ///
    /// # Errors
    ///
    /// Returns the source error without changing state if key generation fails.
    fn reset_identifiers_with<E>(
        &mut self,
        identifier_window_anchor: UtcDay,
        fill_key: impl FnOnce(&mut [u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        let key = IdentityKey::generate_with(fill_key)?;
        self.identity = IdentityState {
            key,
            identifier_window_anchor,
            return_cohort_anchor: None,
        };
        Ok(())
    }

    /// Observe a session on a day accepted by the monotonic clock policy.
    ///
    /// This selects the identifier window and return cohort before mutating
    /// either anchor. The first observed session establishes cohort D0. An
    /// existing cohort keeps its anchor through D30; the first later
    /// observation starts another D0.
    ///
    /// Storage must call this while holding the telemetry lock, after rejecting
    /// a day before the latest-opened-day high-water mark. Any high-water
    /// advancement and this complete session transition belong to one
    /// private-state replacement. That replacement must complete before the
    /// `session_start` row is appended or either returned anchor is used to
    /// derive an identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if `effective_day` precedes either stored anchor. A
    /// conforming storage caller filters this case through its durable day
    /// policy; these checks protect against an incorrect caller or inconsistent
    /// state. Neither anchor changes when validation fails.
    fn observe_session(
        &mut self,
        effective_day: UtcDay,
    ) -> Result<SessionObservation, SessionObservationError> {
        let identifier_window = self.select_identifier_window(effective_day)?;
        let return_cohort = self.select_return_cohort(effective_day)?;

        self.identity.identifier_window_anchor = identifier_window.anchor();
        self.identity.return_cohort_anchor = Some(return_cohort.anchor());

        Ok(SessionObservation {
            identifier_window,
            return_cohort,
        })
    }

    /// Select the identifier window without mutating private state.
    fn select_identifier_window(
        &self,
        effective_day: UtcDay,
    ) -> Result<IdentifierWindowUpdate, SessionObservationError> {
        let anchor = self.identity.identifier_window_anchor;
        let elapsed_days = effective_day.days_since(anchor);

        if elapsed_days < 0 {
            return Err(SessionObservationError::BeforeIdentifierWindow {
                observed_day: effective_day,
                window_anchor: anchor,
            });
        }

        if elapsed_days < IDENTIFIER_WINDOW_DAYS {
            return Ok(IdentifierWindowUpdate::Current { anchor });
        }

        Ok(IdentifierWindowUpdate::Advanced {
            anchor: effective_day,
        })
    }

    /// Select the return cohort without mutating private state.
    fn select_return_cohort(
        &self,
        effective_day: UtcDay,
    ) -> Result<ReturnCohortUpdate, SessionObservationError> {
        let Some(anchor) = self.identity.return_cohort_anchor else {
            return Ok(ReturnCohortUpdate::Started {
                anchor: effective_day,
            });
        };

        let elapsed_days = effective_day.days_since(anchor);
        if elapsed_days < 0 {
            return Err(SessionObservationError::BeforeReturnCohort {
                observed_day: effective_day,
                cohort_anchor: anchor,
            });
        }

        if elapsed_days > i64::from(CohortDay::D30.get()) {
            return Ok(ReturnCohortUpdate::Started {
                anchor: effective_day,
            });
        }

        let day = CohortDay::try_from(elapsed_days)
            .expect("BUG: a session between D0 and D30 must have a valid cohort day");
        Ok(ReturnCohortUpdate::Current { anchor, day })
    }
}

/// Identity and return-cohort selections for one observed session.
#[must_use = "session identity state must be persisted before identifiers are emitted"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionObservation {
    identifier_window: IdentifierWindowUpdate,
    return_cohort: ReturnCohortUpdate,
}

/// Whether selecting an identifier window changed private state.
#[must_use = "an advanced identifier window must be persisted before use"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentifierWindowUpdate {
    /// The existing window remains active.
    Current { anchor: UtcDay },
    /// A new observation-anchored window was started.
    Advanced { anchor: UtcDay },
}

impl IdentifierWindowUpdate {
    /// Return the anchor selected for identifier derivation.
    #[must_use]
    fn anchor(self) -> UtcDay {
        match self {
            Self::Current { anchor } | Self::Advanced { anchor } => anchor,
        }
    }
}

/// Whether selecting a return cohort changed private state.
#[must_use = "a started return cohort must be persisted before use"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnCohortUpdate {
    /// The existing cohort remains active at `day`.
    Current { anchor: UtcDay, day: CohortDay },
    /// A new cohort was started at D0.
    Started { anchor: UtcDay },
}

impl ReturnCohortUpdate {
    /// Return the anchor selected for retention-subject derivation.
    #[must_use]
    fn anchor(self) -> UtcDay {
        match self {
            Self::Current { anchor, .. } | Self::Started { anchor } => anchor,
        }
    }

    /// Return the observed day within the selected cohort.
    #[must_use]
    fn day(self) -> CohortDay {
        match self {
            Self::Current { day, .. } => day,
            Self::Started { .. } => CohortDay::D0,
        }
    }
}

/// An observed session earlier than one of its stored identity anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionObservationError {
    BeforeIdentifierWindow {
        observed_day: UtcDay,
        window_anchor: UtcDay,
    },
    BeforeReturnCohort {
        observed_day: UtcDay,
        cohort_anchor: UtcDay,
    },
}

impl fmt::Display for SessionObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeIdentifierWindow {
                observed_day,
                window_anchor,
            } => write!(
                formatter,
                "observed day {observed_day} precedes the identifier-window anchor {window_anchor}"
            ),
            Self::BeforeReturnCohort {
                observed_day,
                cohort_anchor,
            } => write!(
                formatter,
                "observed session day {observed_day} precedes the return-cohort anchor {cohort_anchor}"
            ),
        }
    }
}

impl std::error::Error for SessionObservationError {}

/// Stable identity material and the dates that define its rotation windows.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct IdentityState {
    #[serde(with = "state_key_hex")]
    key: IdentityKey,
    identifier_window_anchor: UtcDay,
    #[serde(skip_serializing_if = "Option::is_none")]
    return_cohort_anchor: Option<UtcDay>,
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use chrono::NaiveDate;

    use super::{
        CohortDay, IdentifierWindowUpdate, IdentityKey, ReturnCohortUpdate, TelemetryStateV1,
        UtcDay,
    };

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const GENERATED_KEY_BYTE: u8 = 0x42;
    /// Lowercase hexadecimal encoding of 32 [`GENERATED_KEY_BYTE`] bytes.
    const GENERATED_KEY: &str = "4242424242424242424242424242424242424242424242424242424242424242";

    #[derive(Debug, PartialEq, Eq)]
    struct TestKeySourceError;

    fn day(year: i32, month: u32, day: u32) -> UtcDay {
        UtcDay::from_date(NaiveDate::from_ymd_opt(year, month, day).unwrap())
    }

    fn state_with_return_cohort(key: &str) -> String {
        state_with_anchors(key, "2026-09-10", "2026-08-11")
    }

    fn state_with_anchors(
        key: &str,
        identifier_window_anchor: &str,
        return_cohort_anchor: &str,
    ) -> String {
        format!(
            "version = 1\n\n[identity]\nkey = \"{key}\"\nidentifier-window-anchor = \"{identifier_window_anchor}\"\nreturn-cohort-anchor = \"{return_cohort_anchor}\"\n"
        )
    }

    fn state_without_return_cohort(key: &str) -> String {
        state_without_return_cohort_at(key, "2026-09-10")
    }

    fn state_without_return_cohort_at(key: &str, identifier_window_anchor: &str) -> String {
        format!(
            "version = 1\n\n[identity]\nkey = \"{key}\"\nidentifier-window-anchor = \"{identifier_window_anchor}\"\n"
        )
    }

    /// Rejection text for `source`, so each test can pin the reason it failed.
    ///
    /// Destructures rather than calling `unwrap_err`, which would need `Debug` on
    /// the state: the identity key deliberately has no formatting traits.
    fn rejection_message(source: &str) -> String {
        let Err(error) = toml::from_str::<TelemetryStateV1>(source) else {
            panic!("accepted invalid telemetry state:\n{source}");
        };

        error.to_string()
    }

    #[test]
    fn version_one_state_round_trips_in_canonical_form() {
        let source = state_with_return_cohort(KEY);

        let state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert_eq!(serialized, source);
    }

    #[test]
    fn new_state_starts_an_identity_window_without_a_return_cohort() {
        let day = day(2026, 9, 10);

        let state = TelemetryStateV1::new(day).unwrap();

        assert_eq!(state.identity.identifier_window_anchor, day);
        assert!(state.identity.return_cohort_anchor.is_none());
    }

    #[test]
    fn generated_state_has_the_canonical_initial_file_shape() {
        let key = IdentityKey::generate_with::<Infallible>(|bytes| {
            bytes.fill(GENERATED_KEY_BYTE);
            Ok(())
        })
        .unwrap();

        let state = TelemetryStateV1::with_key(day(2026, 9, 10), key);
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_without_return_cohort(GENERATED_KEY);
        assert_eq!(serialized, expected);
    }

    #[test]
    fn identifier_reset_rotates_the_key_resets_the_window_and_clears_the_cohort() {
        let source = state_with_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let reset_day = day(2026, 10, 15);

        state
            .reset_identifiers_with::<Infallible>(reset_day, |bytes| {
                bytes.fill(GENERATED_KEY_BYTE);
                Ok(())
            })
            .unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_without_return_cohort_at(GENERATED_KEY, "2026-10-15");
        assert_eq!(serialized, expected);
    }

    #[test]
    fn failed_identifier_reset_preserves_the_complete_state() {
        let source = state_with_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

        let result = state.reset_identifiers_with(day(2026, 10, 15), |bytes| {
            bytes.fill(GENERATED_KEY_BYTE);
            Err(TestKeySourceError)
        });
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert_eq!(result, Err(TestKeySourceError));
        assert_eq!(serialized, source);
    }

    #[test]
    fn identifier_reset_can_use_operating_system_randomness() {
        let source = state_with_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let reset_day = day(2026, 10, 15);

        state.reset_identifiers(reset_day).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert!(!serialized.contains(KEY));
        assert_eq!(state.identity.identifier_window_anchor, reset_day);
        assert!(state.identity.return_cohort_anchor.is_none());
    }

    #[test]
    fn state_without_an_observed_session_has_no_return_cohort() {
        let source = state_without_return_cohort(KEY);

        let state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert!(state.identity.return_cohort_anchor.is_none());
        assert_eq!(serialized, source);
    }

    #[test]
    fn observations_on_days_zero_through_twenty_nine_keep_the_window() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let anchor = day(2026, 9, 10);

        for observed_day in [anchor, day(2026, 10, 9)] {
            let observation = state.observe_session(observed_day).unwrap();

            assert_eq!(
                observation.identifier_window,
                IdentifierWindowUpdate::Current { anchor }
            );
            assert_eq!(observation.identifier_window.anchor(), anchor);
            assert_eq!(state.identity.identifier_window_anchor, anchor);
        }
    }

    #[test]
    fn observation_on_day_thirty_advances_the_window() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 10, 10);

        let observation = state.observe_session(observed_day).unwrap();

        assert_eq!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced {
                anchor: observed_day
            }
        );
        assert_eq!(observation.return_cohort.day(), CohortDay::D30);
        assert_eq!(state.identity.identifier_window_anchor, observed_day);
    }

    #[test]
    fn observation_after_inactivity_anchors_both_lifecycles_to_the_observation() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 10, 25);

        let observation = state.observe_session(observed_day).unwrap();

        assert_eq!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced {
                anchor: observed_day
            }
        );
        assert_eq!(
            observation.return_cohort,
            ReturnCohortUpdate::Started {
                anchor: observed_day
            }
        );
        assert_eq!(state.identity.identifier_window_anchor, observed_day);
        assert_eq!(state.identity.return_cohort_anchor, Some(observed_day));
    }

    #[test]
    fn window_rollover_preserves_the_key_and_return_cohort() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

        let observation = state.observe_session(day(2026, 10, 10)).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_with_anchors(KEY, "2026-10-10", "2026-09-10");
        assert!(matches!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced { .. }
        ));
        assert_eq!(serialized, expected);
    }

    #[test]
    fn observation_before_the_window_anchor_is_rejected_without_mutation() {
        let source = state_with_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

        let error = state.observe_session(day(2026, 9, 9)).unwrap_err();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert_eq!(
            error.to_string(),
            "observed day 2026-09-09 precedes the identifier-window anchor 2026-09-10"
        );
        assert_eq!(serialized, source);
    }

    #[test]
    fn first_observed_session_starts_d0() {
        let source = state_without_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 9, 10);

        let observation = state.observe_session(observed_day).unwrap();

        assert_eq!(observation.return_cohort.day(), CohortDay::D0);
        assert_eq!(state.identity.return_cohort_anchor, Some(observed_day));
    }

    #[test]
    fn first_session_after_window_expiry_starts_d0_and_advances_the_window() {
        let source = state_without_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 10, 25);

        let observation = state.observe_session(observed_day).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_with_anchors(KEY, "2026-10-25", "2026-10-25");
        assert_eq!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced {
                anchor: observed_day
            }
        );
        assert_eq!(
            observation.return_cohort,
            ReturnCohortUpdate::Started {
                anchor: observed_day
            }
        );
        assert_eq!(serialized, expected);
    }

    #[test]
    fn observations_through_d30_keep_the_existing_cohort() {
        let anchor = day(2026, 8, 11);

        for (window_anchor, observed_day, expected_day) in [
            ("2026-08-11", day(2026, 8, 11), 0_i64),
            ("2026-08-11", day(2026, 8, 12), 1),
            ("2026-09-01", day(2026, 9, 10), 30),
        ] {
            let source = state_with_anchors(KEY, window_anchor, "2026-08-11");
            let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

            let observation = state.observe_session(observed_day).unwrap();

            assert_eq!(
                observation.return_cohort.day(),
                CohortDay::try_from(expected_day).unwrap()
            );
            assert!(matches!(
                observation.identifier_window,
                IdentifierWindowUpdate::Current { .. }
            ));
            assert_eq!(state.identity.return_cohort_anchor, Some(anchor));
        }
    }

    #[test]
    fn cohort_rollover_preserves_the_identifier_window_and_key() {
        let source = state_with_anchors(KEY, "2026-09-01", "2026-08-11");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 9, 11);

        let observation = state.observe_session(observed_day).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_with_anchors(KEY, "2026-09-01", "2026-09-11");
        assert_eq!(
            observation.identifier_window,
            IdentifierWindowUpdate::Current {
                anchor: day(2026, 9, 1)
            }
        );
        assert_eq!(observation.return_cohort.day(), CohortDay::D0);
        assert_eq!(serialized, expected);
    }

    #[test]
    fn both_session_lifecycles_roll_over_in_one_transition() {
        let source = state_with_anchors(KEY, "2026-08-12", "2026-08-11");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observed_day = day(2026, 9, 11);

        let observation = state.observe_session(observed_day).unwrap();
        let serialized = toml::to_string_pretty(&state).unwrap();

        let expected = state_with_anchors(KEY, "2026-09-11", "2026-09-11");
        assert_eq!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced {
                anchor: observed_day
            }
        );
        assert_eq!(
            observation.return_cohort,
            ReturnCohortUpdate::Started {
                anchor: observed_day
            }
        );
        assert_eq!(serialized, expected);
    }

    #[test]
    fn invalid_cohort_day_does_not_partially_advance_the_window() {
        let source = state_with_anchors(KEY, "2026-08-01", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

        let error = state.observe_session(day(2026, 9, 9)).unwrap_err();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert_eq!(
            error.to_string(),
            "observed session day 2026-09-09 precedes the return-cohort anchor 2026-09-10"
        );
        assert_eq!(serialized, source);
    }

    #[test]
    fn future_state_version_is_rejected() {
        let source = state_with_return_cohort(KEY).replacen("version = 1", "version = 2", 1);

        let message = rejection_message(&source);

        assert!(
            message.contains("expected telemetry state version 1, found 2"),
            "unexpected rejection reason: {message}"
        );
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let source = state_with_return_cohort(KEY).replacen(
            "\n[identity]",
            "\nunexpected = true\n\n[identity]",
            1,
        );

        let message = rejection_message(&source);

        assert!(
            message.contains("unknown field `unexpected`"),
            "unexpected rejection reason: {message}"
        );
    }

    #[test]
    fn unknown_identity_field_is_rejected() {
        let mut source = state_with_return_cohort(KEY);
        source.push_str("unexpected = true\n");

        let message = rejection_message(&source);

        assert!(
            message.contains("unknown field `unexpected`"),
            "unexpected rejection reason: {message}"
        );
    }

    #[test]
    fn identity_key_must_have_exactly_64_digits() {
        let one_short = &KEY[..KEY.len() - 1];
        let one_long = format!("{KEY}0");

        for key in ["", one_short, &one_long] {
            let message = rejection_message(&state_with_return_cohort(key));

            assert!(
                message.contains("exactly 64 hexadecimal digits"),
                "accepted or misreported a {}-digit key: {message}",
                key.len()
            );
        }
    }

    #[test]
    fn identity_key_must_use_lowercase_hexadecimal() {
        for key in [KEY.to_uppercase(), KEY.replacen('f', "g", 1)] {
            let message = rejection_message(&state_with_return_cohort(&key));

            assert!(
                message.contains("lowercase hexadecimal digits"),
                "accepted or misreported {key}: {message}"
            );
        }
    }

    #[test]
    fn identity_anchor_must_be_a_canonical_utc_day() {
        let source = state_with_return_cohort(KEY).replacen("2026-09-10", "2026-9-10", 1);

        let message = rejection_message(&source);

        assert!(
            message.contains("UTC day"),
            "unexpected rejection reason: {message}"
        );
    }
}
