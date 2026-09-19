//! Identity-window and return-cohort transitions in private telemetry state.

use std::fmt;

use super::{IdentityState, TelemetryStateV1};
use crate::telemetry::{
    identity::{IdentifierWindowScope, IdentityKey, ReturnCohortScope},
    schema::{CohortDay, UtcDay, UtcSecond},
};

/// Exclusive length of an identifier window in UTC-day positions.
///
/// Offsets 0 through 29 remain in the window; unlike the cohort's inclusive
/// [`CohortDay::D30`] bound, offset 30 starts a new window.
const IDENTIFIER_WINDOW_DAYS: i64 = 30;

impl TelemetryStateV1 {
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
    pub(super) fn reset_identifiers(
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

    /// Observe a session at its captured completion timestamp.
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
    /// Returns an error if the timestamp's UTC day precedes either stored
    /// anchor. A conforming storage caller filters this case through its durable
    /// day policy; these checks protect against an incorrect caller or
    /// inconsistent state. Neither anchor changes when validation fails.
    pub(in crate::telemetry) fn observe_session(
        &mut self,
        completed_at: UtcSecond,
    ) -> Result<SessionObservation, SessionObservationError> {
        let effective_day = completed_at.day();
        let identifier_window = self.select_identifier_window(effective_day)?;
        let return_cohort = self.select_return_cohort(effective_day)?;

        self.identity.identifier_window_anchor = identifier_window.anchor();
        self.identity.return_cohort_anchor = Some(return_cohort.anchor());

        Ok(SessionObservation {
            completed_at,
            identifier_window,
            return_cohort,
        })
    }

    /// Bind a completed session transition to the unchanged private state.
    ///
    /// Storage calls this only after atomically persisting the state changed by
    /// [`Self::observe_session`]. Both selected anchors are checked before any
    /// identity scope is exposed, so an observation whose anchors no longer
    /// match current state is rejected. Storage must still bind immediately
    /// after persistence while holding the same telemetry lock; anchors do not
    /// identify a private-state instance by themselves. When private-state
    /// persistence is implemented, its successful write token will become an
    /// additional required binding input so this ordering is structural.
    ///
    /// # Errors
    ///
    /// Returns an error when either stored anchor differs from the anchor
    /// selected by `observation`.
    pub(in crate::telemetry) fn bind_session_observation(
        &self,
        observation: SessionObservation,
    ) -> Result<BoundSessionObservation<'_>, SessionObservationBindingError> {
        let selected_identifier_window = observation.identifier_window.anchor();
        let current_identifier_window = self.identity.identifier_window_anchor;
        if selected_identifier_window != current_identifier_window {
            return Err(SessionObservationBindingError::IdentifierWindowChanged {
                selected_anchor: selected_identifier_window,
                current_anchor: current_identifier_window,
            });
        }

        let selected_return_cohort = observation.return_cohort.anchor();
        let current_return_cohort = self.identity.return_cohort_anchor;
        if current_return_cohort != Some(selected_return_cohort) {
            return Err(SessionObservationBindingError::ReturnCohortChanged {
                selected_anchor: selected_return_cohort,
                current_anchor: current_return_cohort,
            });
        }

        let identifier_window_scope = self.identifier_window_scope();
        let return_cohort_scope = self
            .return_cohort_scope()
            .expect("BUG: the checked return-cohort anchor must be present");

        Ok(BoundSessionObservation {
            completed_at: observation.completed_at,
            identifier_window: observation.identifier_window,
            return_cohort: observation.return_cohort,
            identifier_window_scope,
            return_cohort_scope,
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
#[derive(Debug, PartialEq, Eq)]
pub(in crate::telemetry) struct SessionObservation {
    completed_at: UtcSecond,
    pub(super) identifier_window: IdentifierWindowUpdate,
    pub(super) return_cohort: ReturnCohortUpdate,
}

/// A persisted session transition bound to both of its identity scopes.
#[must_use = "a bound session observation supplies the session-start identity fields"]
pub(in crate::telemetry) struct BoundSessionObservation<'a> {
    completed_at: UtcSecond,
    identifier_window: IdentifierWindowUpdate,
    return_cohort: ReturnCohortUpdate,
    identifier_window_scope: IdentifierWindowScope<'a>,
    return_cohort_scope: ReturnCohortScope<'a>,
}

impl BoundSessionObservation<'_> {
    /// Return when Symposium completed the observed session-start handling.
    #[must_use]
    pub(in crate::telemetry) fn completed_at(&self) -> UtcSecond {
        self.completed_at
    }

    /// Return identity material bound to the selected identifier window.
    #[must_use]
    pub(in crate::telemetry) fn identifier_window_scope(&self) -> &IdentifierWindowScope<'_> {
        &self.identifier_window_scope
    }

    /// Return identity material bound to the selected return cohort.
    #[must_use]
    pub(in crate::telemetry) fn return_cohort_scope(&self) -> &ReturnCohortScope<'_> {
        &self.return_cohort_scope
    }

    /// Return the observed day within the selected return cohort.
    #[must_use]
    pub(in crate::telemetry) fn cohort_day(&self) -> CohortDay {
        self.return_cohort.day()
    }
}

/// A session transition that no longer matches the current private state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum SessionObservationBindingError {
    IdentifierWindowChanged {
        selected_anchor: UtcDay,
        current_anchor: UtcDay,
    },
    ReturnCohortChanged {
        selected_anchor: UtcDay,
        current_anchor: Option<UtcDay>,
    },
}

impl fmt::Display for SessionObservationBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentifierWindowChanged {
                selected_anchor,
                current_anchor,
            } => write!(
                formatter,
                "session selected identifier-window anchor {selected_anchor}, but current state uses {current_anchor}"
            ),
            Self::ReturnCohortChanged {
                selected_anchor,
                current_anchor: Some(current_anchor),
            } => write!(
                formatter,
                "session selected return-cohort anchor {selected_anchor}, but current state uses {current_anchor}"
            ),
            Self::ReturnCohortChanged {
                selected_anchor,
                current_anchor: None,
            } => write!(
                formatter,
                "session selected return-cohort anchor {selected_anchor}, but current state has no return cohort"
            ),
        }
    }
}

impl std::error::Error for SessionObservationBindingError {}

/// Whether selecting an identifier window changed private state.
#[must_use = "an advanced identifier window must be persisted before use"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdentifierWindowUpdate {
    /// The existing window remains active.
    Current { anchor: UtcDay },
    /// A new observation-anchored window was started.
    Advanced { anchor: UtcDay },
}

impl IdentifierWindowUpdate {
    /// Return the anchor selected for identifier derivation.
    #[must_use]
    pub(super) fn anchor(self) -> UtcDay {
        match self {
            Self::Current { anchor } | Self::Advanced { anchor } => anchor,
        }
    }
}

/// Whether selecting a return cohort changed private state.
#[must_use = "a started return cohort must be persisted before use"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReturnCohortUpdate {
    /// The existing cohort remains active at `day`.
    Current { anchor: UtcDay, day: CohortDay },
    /// A new cohort was started at D0.
    Started { anchor: UtcDay },
}

impl ReturnCohortUpdate {
    /// Return the anchor selected for retention-subject derivation.
    #[must_use]
    pub(super) fn anchor(self) -> UtcDay {
        match self {
            Self::Current { anchor, .. } | Self::Started { anchor } => anchor,
        }
    }

    /// Return the observed day within the selected cohort.
    #[must_use]
    pub(super) fn day(self) -> CohortDay {
        match self {
            Self::Current { day, .. } => day,
            Self::Started { .. } => CohortDay::D0,
        }
    }
}

/// An observed session earlier than one of its stored identity anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum SessionObservationError {
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use chrono::{NaiveDate, TimeZone, Utc};

    use super::*;
    use crate::telemetry::identity::{
        DimensionWriter, IdentityDimension, RetentionDimension, SessionDomain,
    };
    use crate::telemetry::schema::UtcSecond;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const GENERATED_KEY_BYTE: u8 = 0x42;
    /// Lowercase hexadecimal encoding of 32 [`GENERATED_KEY_BYTE`] bytes.
    const GENERATED_KEY: &str = "4242424242424242424242424242424242424242424242424242424242424242";

    #[derive(Debug, PartialEq, Eq)]
    struct TestKeySourceError;

    struct TestSessionDimension;

    impl IdentityDimension for TestSessionDimension {
        type Domain = SessionDomain;

        fn write(&self, writer: &mut DimensionWriter<'_>) {
            writer.field(b"test-session");
        }
    }

    fn day(year: i32, month: u32, day: u32) -> UtcDay {
        UtcDay::from_date(NaiveDate::from_ymd_opt(year, month, day).unwrap())
    }

    fn completion_time(year: i32, month: u32, day: u32) -> UtcSecond {
        UtcSecond::from_datetime(Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap())
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
    fn observations_on_days_zero_through_twenty_nine_keep_the_window() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let anchor = day(2026, 9, 10);

        for completed_at in [completion_time(2026, 9, 10), completion_time(2026, 10, 9)] {
            let observation = state.observe_session(completed_at).unwrap();

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
        let completed_at = completion_time(2026, 10, 10);
        let observed_day = completed_at.day();

        let observation = state.observe_session(completed_at).unwrap();

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
        let completed_at = completion_time(2026, 10, 25);
        let observed_day = completed_at.day();

        let observation = state.observe_session(completed_at).unwrap();

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
    fn observed_session_binds_both_scopes_to_the_selected_anchors() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let old_session = state
            .identifier_window_scope()
            .derive(&TestSessionDimension);
        let old_retention = state
            .return_cohort_scope()
            .expect("fixture has an observed-session cohort")
            .derive(&RetentionDimension);

        let completed_at =
            UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 10, 25, 16, 30, 12).unwrap());
        let observation = state.observe_session(completed_at).unwrap();
        let observation = state.bind_session_observation(observation).unwrap();
        let new_session = observation
            .identifier_window_scope()
            .derive(&TestSessionDimension);
        let new_retention = observation
            .return_cohort_scope()
            .derive(&RetentionDimension);

        assert_ne!(new_session, old_session);
        assert_ne!(new_retention, old_retention);
        assert_eq!(observation.completed_at(), completed_at);
        assert_eq!(observation.cohort_day(), CohortDay::D0);
        assert!(matches!(
            observation.identifier_window,
            IdentifierWindowUpdate::Advanced { .. }
        ));
        assert!(matches!(
            observation.return_cohort,
            ReturnCohortUpdate::Started { .. }
        ));
    }

    #[test]
    fn binding_rejects_an_observation_from_an_older_identifier_window() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let older_observation = state.observe_session(completion_time(2026, 9, 11)).unwrap();
        let current_day = day(2026, 10, 25);
        let _current_observation = state
            .observe_session(completion_time(2026, 10, 25))
            .unwrap();

        let result = state.bind_session_observation(older_observation);

        assert!(matches!(
            result,
            Err(SessionObservationBindingError::IdentifierWindowChanged {
                selected_anchor,
                current_anchor,
            }) if selected_anchor == day(2026, 9, 10) && current_anchor == current_day
        ));
    }

    #[test]
    fn binding_rejects_an_observation_from_an_older_return_cohort() {
        let source = state_with_anchors(KEY, "2026-09-01", "2026-08-11");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let older_observation = state.observe_session(completion_time(2026, 9, 10)).unwrap();
        let current_cohort = day(2026, 9, 11);
        let _current_observation = state.observe_session(completion_time(2026, 9, 11)).unwrap();

        let result = state.bind_session_observation(older_observation);

        assert!(matches!(
            result,
            Err(SessionObservationBindingError::ReturnCohortChanged {
                selected_anchor,
                current_anchor: Some(current_anchor),
            }) if selected_anchor == day(2026, 8, 11) && current_anchor == current_cohort
        ));
    }

    #[test]
    fn binding_rejects_an_observation_after_identifier_reset() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let observation = state.observe_session(completion_time(2026, 9, 11)).unwrap();
        state
            .reset_identifiers_with::<Infallible>(day(2026, 9, 10), |bytes| {
                bytes.fill(GENERATED_KEY_BYTE);
                Ok(())
            })
            .unwrap();

        let result = state.bind_session_observation(observation);

        assert!(matches!(
            result,
            Err(SessionObservationBindingError::ReturnCohortChanged {
                selected_anchor,
                current_anchor: None,
            }) if selected_anchor == day(2026, 9, 10)
        ));
    }

    #[test]
    fn window_rollover_preserves_the_key_and_return_cohort() {
        let source = state_with_anchors(KEY, "2026-09-10", "2026-09-10");
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

        let observation = state
            .observe_session(completion_time(2026, 10, 10))
            .unwrap();
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

        let error = state
            .observe_session(completion_time(2026, 9, 9))
            .unwrap_err();
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
        let completed_at = completion_time(2026, 9, 10);
        let observed_day = completed_at.day();

        let observation = state.observe_session(completed_at).unwrap();

        assert_eq!(observation.return_cohort.day(), CohortDay::D0);
        assert_eq!(state.identity.return_cohort_anchor, Some(observed_day));
    }

    #[test]
    fn first_session_after_window_expiry_starts_d0_and_advances_the_window() {
        let source = state_without_return_cohort(KEY);
        let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();
        let completed_at = completion_time(2026, 10, 25);
        let observed_day = completed_at.day();

        let observation = state.observe_session(completed_at).unwrap();
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

        for (window_anchor, completed_at, expected_day) in [
            ("2026-08-11", completion_time(2026, 8, 11), 0_i64),
            ("2026-08-11", completion_time(2026, 8, 12), 1),
            ("2026-09-01", completion_time(2026, 9, 10), 30),
        ] {
            let source = state_with_anchors(KEY, window_anchor, "2026-08-11");
            let mut state: TelemetryStateV1 = toml::from_str(&source).unwrap();

            let observation = state.observe_session(completed_at).unwrap();

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
        let completed_at = completion_time(2026, 9, 11);

        let observation = state.observe_session(completed_at).unwrap();
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
        let completed_at = completion_time(2026, 9, 11);
        let observed_day = completed_at.day();

        let observation = state.observe_session(completed_at).unwrap();
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

        let error = state
            .observe_session(completion_time(2026, 9, 9))
            .unwrap_err();
        let serialized = toml::to_string_pretty(&state).unwrap();

        assert_eq!(
            error.to_string(),
            "observed session day 2026-09-09 precedes the return-cohort anchor 2026-09-10"
        );
        assert_eq!(serialized, source);
    }
}
