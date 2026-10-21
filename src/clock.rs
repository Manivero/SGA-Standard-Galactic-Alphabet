//! Часы для `time_now_ms()` и `time_monotonic_ms()` — T014, M006 (`std/time`).
//! Полный Feature Contract — см. PROJECT_STATUS.md.
//!
//! Здесь только чистые функции над ИНЪЕКТИРУЕМЫМИ значениями времени: они
//! ничего не знают ни о VM, ни о том, откуда взялись `SystemTime`/`Instant`
//! (настоящие системные часы или значения, собранные тестом). Благодаря этому
//! границы — эпоха Unix, округление вниз, выход за пределы `int` — проверяются
//! юнит-тестами, не дожидаясь реального времени и не трогая системные часы.
//! Чтение настоящих часов (`SystemTime::now()`, `Instant::now()`) — в
//! `runtime::call_builtin` (`"time_now_ms"`) и в
//! `vm::Vm::call_time_monotonic_ms`.
//!
//! Оба результата — целые миллисекунды (округление вниз), помещающиеся в
//! `int` (i64): значение, которое не помещается, — ошибка, а не усечение.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Миллисекунды с 1970-01-01T00:00:00Z (эпоха Unix, UTC) до `now`.
///
/// Время до эпохи Unix и значение, не помещающееся в `int`, — `Err` (без
/// усечения и panic): на настоящей системе оба случая не воспроизводятся, но
/// молча вернуть мусор вместо ошибки нельзя.
pub(crate) fn unix_millis(now: SystemTime) -> Result<i64, String> {
    let since_epoch = now.duration_since(UNIX_EPOCH).map_err(|e| {
        format!(
            "системные часы показывают время до 1970-01-01 UTC (эпохи Unix): \
             раньше неё на {} мс",
            e.duration().as_millis()
        )
    })?;
    millis_to_int(since_epoch)
}

/// Целые миллисекунды от `start` до `now`.
///
/// `now` раньше `start` даёт 0 (сатурация): `Instant` монотонен, так что в
/// работе это недостижимо, но отрицательное значение или panic здесь были бы
/// хуже нуля.
pub(crate) fn elapsed_millis(start: Instant, now: Instant) -> Result<i64, String> {
    millis_to_int(now.saturating_duration_since(start))
}

/// Перевод `Duration` в целые миллисекунды (округление вниз) типа `int`;
/// значение, не помещающееся в i64, — `Err`.
fn millis_to_int(duration: Duration) -> Result<i64, String> {
    i64::try_from(duration.as_millis()).map_err(|_| {
        format!(
            "значение времени ({} мс) не помещается в int (максимум {})",
            duration.as_millis(),
            i64::MAX
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch_plus(duration: Duration) -> SystemTime {
        UNIX_EPOCH + duration
    }

    #[test]
    fn unix_millis_of_the_epoch_is_zero() {
        assert_eq!(unix_millis(UNIX_EPOCH), Ok(0));
    }

    #[test]
    fn unix_millis_rounds_down_to_whole_milliseconds() {
        // 1.9999 с -> 1999, а не 2000: дробная часть миллисекунды отбрасывается.
        assert_eq!(
            unix_millis(epoch_plus(Duration::from_micros(1_999_900))),
            Ok(1999)
        );
        // Меньше миллисекунды — 0; ровно миллисекунда — 1.
        assert_eq!(
            unix_millis(epoch_plus(Duration::from_nanos(999_999))),
            Ok(0)
        );
        assert_eq!(unix_millis(epoch_plus(Duration::from_millis(1))), Ok(1));
    }

    #[test]
    fn unix_millis_counts_from_1970_in_milliseconds() {
        // 2026-10-14T10:24:42.476Z — произвольная известная дата, значение
        // независимо пересчитано вне Rust (Python: datetime.fromtimestamp).
        let known = 1_791_973_482_476_u64;
        assert_eq!(
            unix_millis(epoch_plus(Duration::from_millis(known))),
            Ok(1_791_973_482_476)
        );
        // 2100-01-01T00:00:00Z.
        assert_eq!(
            unix_millis(epoch_plus(Duration::from_secs(4_102_444_800))),
            Ok(4_102_444_800_000)
        );
    }

    #[test]
    fn unix_millis_before_the_epoch_is_an_error_naming_1970() {
        let before_epoch = UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .expect("платформа должна уметь представлять время на секунду раньше эпохи");
        let err = unix_millis(before_epoch).unwrap_err();
        assert!(err.contains("1970"), "получено: {err}");
        assert!(err.contains("1000 мс"), "получено: {err}");
    }

    #[test]
    fn unix_millis_beyond_the_int_range_is_an_error_not_a_panic() {
        let max_ms = u64::try_from(i64::MAX).unwrap();
        // Если платформа не умеет представлять такое время (например, Windows),
        // граница int проверяется платформо-независимо в
        // `millis_to_int_accepts_i64_max_and_rejects_one_more`.
        let Some(at_max) = UNIX_EPOCH.checked_add(Duration::from_millis(max_ms)) else {
            return;
        };
        assert_eq!(unix_millis(at_max), Ok(i64::MAX));
        let Some(over) = UNIX_EPOCH.checked_add(Duration::from_millis(max_ms + 1)) else {
            return;
        };
        let err = unix_millis(over).unwrap_err();
        assert!(err.contains("не помещается в int"), "получено: {err}");
    }

    #[test]
    fn elapsed_millis_of_equal_instants_is_zero() {
        let now = Instant::now();
        assert_eq!(elapsed_millis(now, now), Ok(0));
    }

    #[test]
    fn elapsed_millis_rounds_down_to_whole_milliseconds() {
        let start = Instant::now();
        assert_eq!(
            elapsed_millis(start, start + Duration::from_micros(1_999_900)),
            Ok(1999)
        );
        assert_eq!(
            elapsed_millis(start, start + Duration::from_nanos(999_999)),
            Ok(0)
        );
        assert_eq!(
            elapsed_millis(start, start + Duration::from_millis(30)),
            Ok(30)
        );
    }

    #[test]
    fn elapsed_millis_with_now_before_start_saturates_to_zero() {
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(5);
        assert_eq!(elapsed_millis(later, earlier), Ok(0));
    }

    #[test]
    fn millis_to_int_accepts_i64_max_and_rejects_one_more() {
        let max_ms = u64::try_from(i64::MAX).unwrap();
        assert_eq!(millis_to_int(Duration::from_millis(max_ms)), Ok(i64::MAX));
        let err = millis_to_int(Duration::from_millis(max_ms + 1)).unwrap_err();
        assert!(err.contains("не помещается в int"), "получено: {err}");
        assert!(
            millis_to_int(Duration::MAX).is_err(),
            "Duration::MAX не должен усекаться до int"
        );
    }

    /// Целые миллисекунды с эпохи Unix по «сырым» показаниям часов — независимый
    /// от `unix_millis` способ получить то же число (для скобки в тестах).
    fn raw_unix_millis() -> i64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("системные часы должны показывать время после 1970-01-01");
        i64::try_from(since_epoch.as_millis()).expect("время помещается в i64")
    }

    #[test]
    fn the_real_wall_clock_lies_between_two_raw_readings() {
        // Связка с настоящими часами: результат лежит между сырыми показаниями
        // тех же часов, снятыми до и после (скобка — свойство, а не точное
        // значение, иначе тест недетерминирован).
        let before = raw_unix_millis();
        let value = unix_millis(SystemTime::now()).unwrap();
        let after = raw_unix_millis();
        assert!(
            (before..=after).contains(&value),
            "{before} <= {value} <= {after}"
        );
    }

    #[test]
    fn the_real_monotonic_clock_never_exceeds_the_time_that_really_passed() {
        let start = Instant::now();
        let value = elapsed_millis(start, Instant::now()).unwrap();
        let passed = i64::try_from(start.elapsed().as_millis()).unwrap();
        assert!((0..=passed).contains(&value), "0 <= {value} <= {passed}");
    }
}
