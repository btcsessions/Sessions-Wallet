//! Spending budgets: a cap per payment and a cap per period.
//!
//! Periods are anchored on the connection's creation time, not on calendar
//! boundaries, so they behave the same in every time zone: a daily budget
//! created at 14:05 renews every day at 14:05 (UTC-equivalent instant), a
//! monthly one on the same day of each month (clamped to the month's last
//! day, without drifting: Jan 31 → Feb 28 → Mar 31).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    #[default]
    Monthly,
    Yearly,
    /// Never renews: a lifetime cap for the connection.
    Total,
}

impl BudgetPeriod {
    /// NWC-13 `renewal_period`.
    pub fn renewal_period(self) -> &'static str {
        match self {
            BudgetPeriod::Daily => "daily",
            BudgetPeriod::Weekly => "weekly",
            BudgetPeriod::Monthly => "monthly",
            BudgetPeriod::Yearly => "yearly",
            BudgetPeriod::Total => "never",
        }
    }
}

/// The user's limits for one connection (sats, as the UI shows them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Budget {
    /// Largest single payment, in sats. `None`: no per-payment cap.
    #[serde(default)]
    pub max_per_payment_sat: Option<u64>,
    /// Total that may be spent per `period`, in sats (fees included).
    /// `None`: no periodic cap.
    #[serde(default)]
    pub limit_sat: Option<u64>,
    #[serde(default)]
    pub period: BudgetPeriod,
}

impl Budget {
    pub fn unlimited() -> Self {
        Budget::default()
    }

    pub fn is_unlimited(&self) -> bool {
        self.max_per_payment_sat.is_none() && self.limit_sat.is_none()
    }
}

/// What has been spent in the current period. Kept by the service and
/// handed back to the app (through the store) to persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BudgetUsage {
    /// Millisats spent in the period starting at `period_start`, fees included.
    #[serde(default)]
    pub spent_msat: u64,
    /// Unix seconds.
    #[serde(default)]
    pub period_start: u64,
}

impl BudgetUsage {
    /// Move to the period containing `now`, forgetting what was spent in an
    /// earlier one.
    pub fn roll(&mut self, period: BudgetPeriod, anchor: u64, now: u64) {
        let (start, _) = period_bounds(period, anchor, now);
        if start > self.period_start && period != BudgetPeriod::Total {
            self.spent_msat = 0;
        }
        self.period_start = start;
    }
}

const DAY: u64 = 86_400;

/// `[start, end)` of the period containing `now` (`end` is `None` for
/// [`BudgetPeriod::Total`]). Before the anchor, the first period.
pub fn period_bounds(period: BudgetPeriod, anchor: u64, now: u64) -> (u64, Option<u64>) {
    let now = now.max(anchor);
    match period {
        BudgetPeriod::Total => (anchor, None),
        BudgetPeriod::Daily | BudgetPeriod::Weekly => {
            let len = if period == BudgetPeriod::Daily {
                DAY
            } else {
                7 * DAY
            };
            let start = anchor + (now - anchor) / len * len;
            (start, Some(start + len))
        }
        BudgetPeriod::Monthly | BudgetPeriod::Yearly => {
            let step = if period == BudgetPeriod::Monthly {
                1
            } else {
                12
            };
            let (ay, am, _) = civil_from_days((anchor / DAY) as i64);
            let (ny, nm, _) = civil_from_days((now / DAY) as i64);
            let months = (ny - ay) * 12 + (nm as i64 - am as i64);
            let mut k = months / step;
            if add_months(anchor, k * step) > now {
                k -= 1;
            }
            (
                add_months(anchor, k * step),
                Some(add_months(anchor, (k + 1) * step)),
            )
        }
    }
}

/// `t` moved `months` calendar months later, day clamped to the month's end.
fn add_months(t: u64, months: i64) -> u64 {
    let secs = t % DAY;
    let (y, m, d) = civil_from_days((t / DAY) as i64);
    let total = y * 12 + (m as i64 - 1) + months;
    let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    let nd = d.min(days_in_month(ny, nm));
    days_from_civil(ny, nm, nd) as u64 * DAY + secs
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

// Howard Hinnant's civil calendar algorithms (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i64, m: u32, d: u32, h: u64) -> u64 {
        days_from_civil(y, m, d) as u64 * DAY + h * 3600
    }

    #[test]
    fn calendar_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(ts(2024, 2, 29, 0), 1_709_164_800);
        for z in [0i64, 59, 60, 365, 10_957, 19_782, 20_000, 30_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn daily_and_weekly() {
        let a = ts(2026, 10, 1, 14);
        assert_eq!(period_bounds(BudgetPeriod::Daily, a, a), (a, Some(a + DAY)));
        assert_eq!(
            period_bounds(BudgetPeriod::Daily, a, a + DAY - 1),
            (a, Some(a + DAY))
        );
        assert_eq!(
            period_bounds(BudgetPeriod::Daily, a, a + 3 * DAY + 5),
            (a + 3 * DAY, Some(a + 4 * DAY))
        );
        assert_eq!(
            period_bounds(BudgetPeriod::Weekly, a, a + 8 * DAY),
            (a + 7 * DAY, Some(a + 14 * DAY))
        );
        // Before the anchor (clock skew): the first period.
        assert_eq!(period_bounds(BudgetPeriod::Daily, a, a - 10).0, a);
    }

    #[test]
    fn monthly_clamps_without_drift() {
        let a = ts(2026, 1, 31, 10);
        let b = |now| period_bounds(BudgetPeriod::Monthly, a, now);
        assert_eq!(b(ts(2026, 2, 15, 0)), (a, Some(ts(2026, 2, 28, 10))));
        assert_eq!(
            b(ts(2026, 2, 28, 10)),
            (ts(2026, 2, 28, 10), Some(ts(2026, 3, 31, 10)))
        );
        assert_eq!(
            b(ts(2026, 3, 31, 9)),
            (ts(2026, 2, 28, 10), Some(ts(2026, 3, 31, 10)))
        );
        assert_eq!(
            b(ts(2026, 4, 30, 11)),
            (ts(2026, 4, 30, 10), Some(ts(2026, 5, 31, 10)))
        );
        assert_eq!(
            b(ts(2027, 1, 1, 0)),
            (ts(2026, 12, 31, 10), Some(ts(2027, 1, 31, 10)))
        );
        // Leap year.
        assert_eq!(b(ts(2028, 2, 29, 12)).0, ts(2028, 2, 29, 10));
    }

    #[test]
    fn yearly_and_total() {
        let a = ts(2024, 2, 29, 0);
        assert_eq!(
            period_bounds(BudgetPeriod::Yearly, a, ts(2025, 3, 1, 0)),
            (ts(2025, 2, 28, 0), Some(ts(2026, 2, 28, 0)))
        );
        assert_eq!(
            period_bounds(BudgetPeriod::Total, a, ts(2030, 1, 1, 0)),
            (a, None)
        );
    }

    #[test]
    fn usage_rolls_over() {
        let a = ts(2026, 10, 1, 0);
        let mut u = BudgetUsage::default();
        u.roll(BudgetPeriod::Daily, a, a + 10);
        assert_eq!(u.period_start, a);
        u.spent_msat = 5_000;
        u.roll(BudgetPeriod::Daily, a, a + DAY - 1);
        assert_eq!(u.spent_msat, 5_000);
        u.roll(BudgetPeriod::Daily, a, a + DAY);
        assert_eq!(u.spent_msat, 0);
        assert_eq!(u.period_start, a + DAY);

        let mut t = BudgetUsage {
            spent_msat: 9,
            period_start: 0,
        };
        t.roll(BudgetPeriod::Total, a, a + 400 * DAY);
        assert_eq!(t.spent_msat, 9);
    }
}
