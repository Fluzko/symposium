//! Shared schema types for cumulative telemetry metrics.

use std::{fmt, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

const LATENCY_BOUNDS_MS: [u64; 8] = [5, 10, 25, 50, 100, 250, 500, 1_000];
const LATENCY_BUCKET_COUNT: usize = LATENCY_BOUNDS_MS.len() + 1;

/// Convert a duration to the contract's whole-millisecond representation.
///
/// Sub-millisecond precision is truncated. Durations outside the wire type's
/// range saturate instead of wrapping to an unrelated smaller value.
#[must_use]
pub(in crate::telemetry) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Fixed millisecond histogram shared by cumulative telemetry rows.
///
/// The bounds are part of the version 1 wire contract rather than runtime
/// state. Keeping only the counters in this type makes changed bounds
/// unrepresentable after deserialization.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct LatencyHistogram {
    counts: [u64; LATENCY_BUCKET_COUNT],
}

impl LatencyHistogram {
    /// Increment the bucket containing `duration` after converting it to the
    /// contract's whole-millisecond representation.
    ///
    /// # Errors
    ///
    /// Returns [`LatencyHistogramError::BucketCountOverflow`] when the
    /// selected counter cannot be incremented. The histogram is unchanged on
    /// failure.
    #[must_use = "counter overflow must drop the containing telemetry update"]
    pub(in crate::telemetry) fn checked_record(
        &mut self,
        duration: Duration,
    ) -> Result<(), LatencyHistogramError> {
        let duration_ms = duration_millis(duration);
        let bucket = LATENCY_BOUNDS_MS.partition_point(|bound| duration_ms > *bound);
        let next = self.counts[bucket]
            .checked_add(1)
            .ok_or(LatencyHistogramError::BucketCountOverflow { bucket })?;

        self.counts[bucket] = next;
        Ok(())
    }

    /// Return the sum of every bucket, or `None` if the total exceeds `u64`.
    #[must_use]
    pub(in crate::telemetry) fn checked_total(&self) -> Option<u64> {
        self.counts
            .iter()
            .try_fold(0_u64, |total, count| total.checked_add(*count))
    }
}

/// A cumulative latency counter that cannot be represented in `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum LatencyHistogramError {
    BucketCountOverflow { bucket: usize },
}

impl fmt::Display for LatencyHistogramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BucketCountOverflow { bucket } => {
                write!(formatter, "latency histogram bucket {bucket} overflows u64")
            }
        }
    }
}

impl std::error::Error for LatencyHistogramError {}

impl Serialize for LatencyHistogram {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RawLatencyHistogram {
            bounds: LATENCY_BOUNDS_MS,
            counts: self.counts,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LatencyHistogram {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawLatencyHistogram::deserialize(deserializer)?;

        if raw.bounds != LATENCY_BOUNDS_MS {
            return Err(D::Error::custom(format_args!(
                "expected latency histogram bounds {LATENCY_BOUNDS_MS:?}, found {:?}",
                raw.bounds
            )));
        }

        Ok(Self { counts: raw.counts })
    }
}

/// Strict wire representation of a latency histogram.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLatencyHistogram {
    bounds: [u64; LATENCY_BOUNDS_MS.len()],
    counts: [u64; LATENCY_BUCKET_COUNT],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_histogram_matches_the_recorded_data_contract() {
        let example = super::super::recorded_data_example_block(
            "### Rules shared by hook aggregates",
            "```json",
        )
        .trim();

        let histogram = serde_json::from_str::<LatencyHistogram>(example).unwrap();
        let encoded = serde_json::to_string(&histogram).unwrap();

        assert_eq!(histogram, LatencyHistogram::default());
        assert_eq!(encoded, example);
    }

    #[test]
    fn durations_use_the_contract_bucket_boundaries() {
        let mut histogram = LatencyHistogram::default();
        let durations_ms = [
            0,
            5,
            6,
            10,
            11,
            25,
            26,
            50,
            51,
            100,
            101,
            250,
            251,
            500,
            501,
            1_000,
            1_001,
            u64::MAX,
        ];

        for duration_ms in durations_ms {
            histogram
                .checked_record(Duration::from_millis(duration_ms))
                .unwrap();
        }

        assert_eq!(histogram.counts, [2; LATENCY_BUCKET_COUNT]);
    }

    #[test]
    fn duration_conversion_truncates_and_saturates() {
        let sub_millisecond = Duration::from_nanos(999_999);
        let fractional_millisecond = Duration::from_micros(5_999);
        let outside_u64_milliseconds = Duration::from_secs(u64::MAX / 1_000 + 1);

        assert_eq!(duration_millis(sub_millisecond), 0);
        assert_eq!(duration_millis(fractional_millisecond), 5);
        assert_eq!(duration_millis(outside_u64_milliseconds), u64::MAX);
    }

    #[test]
    fn histogram_rejects_changed_bounds() {
        let json = r#"{"bounds":[4,10,25,50,100,250,500,1000],"counts":[0,0,0,0,0,0,0,0,0]}"#;

        let error = serde_json::from_str::<LatencyHistogram>(json).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("expected latency histogram bounds"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn histogram_rejects_variable_array_lengths() {
        let cases = [
            r#"{"bounds":[5,10,25,50,100,250,500],"counts":[0,0,0,0,0,0,0,0,0]}"#,
            r#"{"bounds":[5,10,25,50,100,250,500,1000,2000],"counts":[0,0,0,0,0,0,0,0,0]}"#,
            r#"{"bounds":[5,10,25,50,100,250,500,1000],"counts":[0,0,0,0,0,0,0,0]}"#,
            r#"{"bounds":[5,10,25,50,100,250,500,1000],"counts":[0,0,0,0,0,0,0,0,0,0]}"#,
        ];

        for json in cases {
            assert!(
                serde_json::from_str::<LatencyHistogram>(json).is_err(),
                "accepted variable histogram shape {json}"
            );
        }
    }

    #[test]
    fn histogram_rejects_missing_and_unknown_fields() {
        let cases = [
            r#"{"counts":[0,0,0,0,0,0,0,0,0]}"#,
            r#"{"bounds":[5,10,25,50,100,250,500,1000]}"#,
            r#"{"bounds":[5,10,25,50,100,250,500,1000],"counts":[0,0,0,0,0,0,0,0,0],"future":true}"#,
        ];

        for json in cases {
            assert!(
                serde_json::from_str::<LatencyHistogram>(json).is_err(),
                "accepted invalid histogram object {json}"
            );
        }
    }

    #[test]
    fn recording_rejects_overflow_without_mutation() {
        let mut histogram = LatencyHistogram {
            counts: [u64::MAX, 1, 2, 3, 4, 5, 6, 7, 8],
        };
        let before = histogram;

        let result = histogram.checked_record(Duration::from_millis(5));

        assert_eq!(
            result,
            Err(LatencyHistogramError::BucketCountOverflow { bucket: 0 })
        );
        assert_eq!(histogram, before);
    }

    #[test]
    fn histogram_total_is_checked_for_overflow() {
        let representable = LatencyHistogram {
            counts: [1, 2, 3, 4, 5, 6, 7, 8, 9],
        };
        let overflowing = LatencyHistogram {
            counts: [u64::MAX, 1, 0, 0, 0, 0, 0, 0, 0],
        };

        assert_eq!(representable.checked_total(), Some(45));
        assert_eq!(overflowing.checked_total(), None);
    }
}
