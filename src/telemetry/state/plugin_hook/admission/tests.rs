use chrono::{TimeZone as _, Utc};

use super::*;
use crate::telemetry::{
    schema::{PluginScope, PublicPluginCoordinate, UtcSecond},
    state::{IDENTIFIER_WINDOW_TEST_STATE, TelemetryStateV1},
};

fn state() -> TelemetryStateV1 {
    toml::from_str(IDENTIFIER_WINDOW_TEST_STATE).unwrap()
}

fn recording_at(
    state: &mut TelemetryStateV1,
    day: u32,
    hour: u32,
) -> BoundRecordingObservation<'_> {
    let completed_at =
        UtcSecond::from_datetime(Utc.with_ymd_and_hms(2026, 8, day, hour, 2, 11).unwrap());
    let observation = state.observe_recording(completed_at).unwrap();
    state.bind_recording_observation(observation).unwrap()
}

fn public_plugin(name: &str) -> PublicPluginCoordinate {
    serde_json::from_value(serde_json::json!({
        "source": "symposium-recommendations",
        "name": name,
    }))
    .unwrap()
}

fn select<'a>(
    store: &'a mut PluginHookAggregateStore,
    recording: &BoundRecordingObservation<'_>,
    attribution: PluginHookAttribution,
) -> SelectedPluginHookAggregate<'a> {
    store
        .select(
            recording,
            HookAgent::Claude,
            HookSurface::PreToolUse,
            attribution,
        )
        .unwrap()
}

#[test]
fn repeated_selection_reuses_the_same_private_entry() {
    let plugin = public_plugin("example-tools");
    let mut state = state();
    let first_recording = recording_at(&mut state, 3, 10);
    let mut store = PluginHookAggregateStore::new(first_recording.day());
    let first_event_id = select(
        &mut store,
        &first_recording,
        PluginHookAttribution::Public(plugin.clone()),
    )
    .event_id();
    drop(first_recording);
    let second_recording = recording_at(&mut state, 3, 11);

    let second = select(
        &mut store,
        &second_recording,
        PluginHookAttribution::Public(plugin),
    );

    assert_eq!(second.event_id(), first_event_id);
    assert_eq!(second.bucket().scope(), PluginScope::Public);
    assert_eq!(store.len(), 1);
}

#[test]
fn identifier_reset_discards_entries_from_the_previous_epoch() {
    let mut state = state();
    let recording = recording_at(&mut state, 3, 10);
    let day = recording.day();
    let mut store = PluginHookAggregateStore::new(day);
    let first_event_id = select(&mut store, &recording, PluginHookAttribution::Unnamed).event_id();
    drop(recording);
    state.reset_identifiers(day).unwrap();
    store.reset_identifier_epoch();
    let recording = recording_at(&mut state, 3, 11);

    let selected = select(&mut store, &recording, PluginHookAttribution::Unnamed);

    assert_ne!(selected.event_id(), first_event_id);
    assert_eq!(store.len(), 1);
}

#[test]
fn clear_discards_current_entries() {
    let mut state = state();
    let recording = recording_at(&mut state, 3, 10);
    let mut store = PluginHookAggregateStore::new(recording.day());
    select(&mut store, &recording, PluginHookAttribution::Unnamed);
    assert_eq!(store.len(), 1);

    store.clear();

    assert_eq!(store.len(), 0);
}

#[test]
fn day_rollover_discards_entries_from_the_previous_day() {
    let mut state = state();
    let first_recording = recording_at(&mut state, 3, 10);
    let mut store = PluginHookAggregateStore::new(first_recording.day());
    let first_event_id =
        select(&mut store, &first_recording, PluginHookAttribution::Unnamed).event_id();
    drop(first_recording);
    let next_recording = recording_at(&mut state, 4, 10);

    let selected = select(&mut store, &next_recording, PluginHookAttribution::Unnamed);

    assert_ne!(selected.event_id(), first_event_id);
    assert_eq!(store.len(), 1);
}

#[test]
fn an_older_observation_is_rejected_without_changing_the_store() {
    let mut telemetry_state = state();
    let older_recording = recording_at(&mut telemetry_state, 3, 10);
    let observed = older_recording.day();
    drop(older_recording);
    let newer_recording = recording_at(&mut telemetry_state, 4, 10);
    let current = newer_recording.day();
    let mut store = PluginHookAggregateStore::new(current);
    select(&mut store, &newer_recording, PluginHookAttribution::Unnamed);
    let before = store.clone();

    let mut older_state = state();
    let older_recording = recording_at(&mut older_state, 3, 11);
    let result = store.select(
        &older_recording,
        HookAgent::Claude,
        HookSurface::PreToolUse,
        PluginHookAttribution::Unnamed,
    );

    assert_eq!(
        result.err(),
        Some(PluginHookAdmissionError::DayBeforeCurrent(
            DayBeforeCurrent { current, observed }
        ))
    );
    assert_eq!(store, before);
}
