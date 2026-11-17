//! Core's nonnegative, i64-saturated Unix epoch millisecond clock.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unix epoch milliseconds. Pre-epoch is zero; overflow saturates at i64::MAX.
pub fn now_ms() -> i64 {
    epoch_ms(SystemTime::now().duration_since(UNIX_EPOCH).ok())
}

/// The same clock and range as `now_ms`, represented as u64; maximum is i64::MAX.
pub fn now_ms_u64() -> u64 {
    now_ms() as u64
}

fn epoch_ms(elapsed: Option<Duration>) -> i64 {
    elapsed
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_epoch_is_zero_and_fractional_milliseconds_floor() {
        // None covers every pre-epoch distance, including less than one millisecond.
        assert_eq!(epoch_ms(None), 0);
        // A full second constructs a portable SystemTimeError, including on Windows.
        let pre_epoch = UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_secs(1));
        assert!(pre_epoch.is_err());
        assert_eq!(epoch_ms(pre_epoch.ok()), 0);
        for (nanos, expected) in [(0, 0), (1, 0), (999_999, 0), (1_000_000, 1), (1_999_999, 1)] {
            let actual = epoch_ms(Some(Duration::new(0, nanos)));
            assert_eq!(actual, expected);
            assert_eq!(actual as u64, expected as u64);
        }
    }

    #[test]
    fn epoch_milliseconds_saturate_at_the_same_i64_ceiling_in_both_representations() {
        // All inputs and expected results are literal; none is truncated through u64 milliseconds.
        let cases: [(Duration, u128, i64, u64); 8] = [
            (Duration::new(0, 0), 0, 0, 0),
            (Duration::new(0, 1_000_000), 1, 1, 1),
            (
                Duration::new(9_223_372_036_854_775, 806_000_000),
                9_223_372_036_854_775_806,
                9_223_372_036_854_775_806,
                9_223_372_036_854_775_806,
            ),
            (
                Duration::new(9_223_372_036_854_775, 807_000_000),
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
            ),
            (
                Duration::new(9_223_372_036_854_775, 808_000_000),
                9_223_372_036_854_775_808,
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
            ),
            (
                Duration::new(18_446_744_073_709_551, 615_000_000),
                18_446_744_073_709_551_615,
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
            ),
            (
                Duration::new(18_446_744_073_709_551, 616_000_000),
                18_446_744_073_709_551_616,
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
            ),
            (
                Duration::new(18_446_744_073_709_551, 617_000_000),
                18_446_744_073_709_551_617,
                9_223_372_036_854_775_807,
                9_223_372_036_854_775_807,
            ),
        ];
        for (elapsed, input_ms, expected_i64, expected_u64) in cases {
            assert_eq!(elapsed.as_millis(), input_ms);
            let actual = epoch_ms(Some(elapsed));
            assert!(actual >= 0);
            assert_eq!(actual, expected_i64);
            assert_eq!(actual as u64, expected_u64);
        }
    }
}
