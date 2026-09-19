//! Daily admission budget shared by named aggregate families.

use std::fmt;

use crate::telemetry::schema::UtcDay;

/// Maximum public rows one aggregate family may admit in a UTC day.
pub(super) const MAX_PUBLIC_ROWS_PER_DAY: u64 = 128;

/// Whether a new public aggregate keeps its identity or joins overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicRowAdmission {
    Public,
    Overflow,
}

/// Change observed while selecting the active budget day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BudgetDayUpdate {
    Current,
    Advanced,
}

/// Independent daily allowance for one family of public aggregate rows.
///
/// Callers check for an existing aggregate before consuming this budget.
/// Identifier reset deliberately does not mutate it. Day rollover and clear
/// are the only operations that restore the full allowance.
/// Persistence must source this day from the latest-opened-day high-water mark
/// rather than store a second independent monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DailyPublicRowBudget {
    day: UtcDay,
    admitted: u64,
}

impl DailyPublicRowBudget {
    #[must_use]
    pub(super) const fn new(day: UtcDay) -> Self {
        Self { day, admitted: 0 }
    }

    /// Select a day, restoring the allowance after forward UTC-day rollover.
    ///
    /// # Errors
    ///
    /// Returns a budget-day error if the observed day precedes the budget's
    /// monotonic UTC day.
    pub(super) fn select_day(
        &mut self,
        day: UtcDay,
    ) -> Result<BudgetDayUpdate, BudgetDayBeforeCurrent> {
        if day < self.day {
            return Err(BudgetDayBeforeCurrent {
                current: self.day,
                observed: day,
            });
        }
        if day == self.day {
            return Ok(BudgetDayUpdate::Current);
        }

        self.day = day;
        self.admitted = 0;
        Ok(BudgetDayUpdate::Advanced)
    }

    /// Consume one allowance slot for a new public aggregate.
    #[must_use]
    pub(super) fn admit_new(&mut self) -> PublicRowAdmission {
        if self.admitted >= MAX_PUBLIC_ROWS_PER_DAY {
            return PublicRowAdmission::Overflow;
        }

        self.admitted = self
            .admitted
            .checked_add(1)
            .expect("BUG: the public-row allowance is bounded before incrementing");
        PublicRowAdmission::Public
    }

    /// Return whether a new public aggregate must join overflow.
    #[must_use]
    pub(super) const fn is_exhausted(self) -> bool {
        self.admitted >= MAX_PUBLIC_ROWS_PER_DAY
    }

    /// Restore the current day's allowance after telemetry clear.
    pub(super) fn clear(&mut self) {
        self.admitted = 0;
    }

    #[cfg(test)]
    #[must_use]
    pub(super) const fn admitted(self) -> u64 {
        self.admitted
    }
}

/// An observation dated before a family's active public-row budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BudgetDayBeforeCurrent {
    pub(super) current: UtcDay,
    pub(super) observed: UtcDay,
}

impl fmt::Display for BudgetDayBeforeCurrent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "public-row budget day {} precedes active day {}",
            self.observed, self.current
        )
    }
}

impl std::error::Error for BudgetDayBeforeCurrent {}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn day(day: u32) -> UtcDay {
        UtcDay::from_date(NaiveDate::from_ymd_opt(2026, 8, day).unwrap())
    }

    #[test]
    fn the_128th_public_row_is_admitted_and_the_129th_overflows() {
        let mut budget = DailyPublicRowBudget::new(day(3));

        for admitted in 1..=MAX_PUBLIC_ROWS_PER_DAY {
            assert_eq!(budget.admit_new(), PublicRowAdmission::Public);
            assert_eq!(budget.admitted(), admitted);
        }
        assert_eq!(budget.admit_new(), PublicRowAdmission::Overflow);

        assert_eq!(budget.admitted(), MAX_PUBLIC_ROWS_PER_DAY);
    }

    #[test]
    fn a_forward_day_restores_the_allowance_but_the_current_day_does_not() {
        let mut budget = DailyPublicRowBudget::new(day(3));
        assert_eq!(budget.admit_new(), PublicRowAdmission::Public);

        assert_eq!(budget.select_day(day(3)), Ok(BudgetDayUpdate::Current));
        assert_eq!(budget.admitted(), 1);
        assert_eq!(budget.select_day(day(4)), Ok(BudgetDayUpdate::Advanced));

        assert_eq!(budget.admitted(), 0);
    }

    #[test]
    fn an_older_day_is_rejected_without_changing_the_budget() {
        let mut budget = DailyPublicRowBudget::new(day(4));
        assert_eq!(budget.admit_new(), PublicRowAdmission::Public);
        let before = budget;

        let result = budget.select_day(day(3));

        assert_eq!(
            result,
            Err(BudgetDayBeforeCurrent {
                current: day(4),
                observed: day(3),
            })
        );
        assert_eq!(budget, before);
    }

    #[test]
    fn clear_restores_the_current_days_allowance() {
        let mut budget = DailyPublicRowBudget::new(day(3));
        assert_eq!(budget.admit_new(), PublicRowAdmission::Public);

        budget.clear();

        assert_eq!(budget.admitted(), 0);
        assert_eq!(budget.select_day(day(3)), Ok(BudgetDayUpdate::Current));
    }
}
