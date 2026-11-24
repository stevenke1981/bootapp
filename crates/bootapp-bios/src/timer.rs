// INT 1Ah/AH=00h counts ticks since midnight, not across a u32 rollover.
pub const BIOS_TICKS_PER_DAY: u32 = 0x0018_00b0;
/// Automatic boot delay: 91 ticks at 18.2 Hz is five seconds.
pub const AUTO_BOOT_TICKS: u32 = 91;

/// Elapsed ticks over at most one BIOS midnight transition.
/// The automatic-boot interval is only 91 ticks, far shorter than one day.
pub fn elapsed_ticks(started: u32, now: u32) -> u32 {
    let started = started % BIOS_TICKS_PER_DAY;
    let now = now % BIOS_TICKS_PER_DAY;
    if now >= started {
        now - started
    } else {
        BIOS_TICKS_PER_DAY - started + now
    }
}

/// Whole seconds left before automatic boot, rounded up (18.2 ticks/second).
/// Returns 0 once `elapsed` reaches `timeout`.
pub fn remaining_seconds(elapsed: u32, timeout: u32) -> u32 {
    let left = u64::from(timeout.saturating_sub(elapsed));
    // ceil(left / 18.2) == ceil(left * 10 / 182); u64 prevents overflow.
    (left * 10).div_ceil(182) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_day_timeout_boundary() {
        assert_eq!(elapsed_ticks(1_000, 1_000), 0);
        assert_eq!(elapsed_ticks(1_000, 1_090), 90);
        assert_eq!(elapsed_ticks(1_000, 1_091), 91);
    }

    #[test]
    fn midnight_does_not_expire_early() {
        let started = BIOS_TICKS_PER_DAY - 20;
        assert_eq!(elapsed_ticks(started, 0), 20);
        assert_eq!(elapsed_ticks(started, 70), 90);
        assert!(elapsed_ticks(started, 70) < 91);
        assert_eq!(elapsed_ticks(started, 71), 91);
    }

    #[test]
    fn normalizes_day_boundary_value() {
        assert_eq!(elapsed_ticks(BIOS_TICKS_PER_DAY - 1, BIOS_TICKS_PER_DAY), 1);
        assert_eq!(elapsed_ticks(BIOS_TICKS_PER_DAY, 91), 91);
    }

    #[test]
    fn countdown_starts_at_five_and_reaches_zero_on_timeout() {
        assert_eq!(remaining_seconds(0, AUTO_BOOT_TICKS), 5);
        assert_eq!(remaining_seconds(18, AUTO_BOOT_TICKS), 5);
        assert_eq!(remaining_seconds(19, AUTO_BOOT_TICKS), 4);
        assert_eq!(remaining_seconds(90, AUTO_BOOT_TICKS), 1);
        assert_eq!(remaining_seconds(91, AUTO_BOOT_TICKS), 0);
        assert_eq!(remaining_seconds(u32::MAX, AUTO_BOOT_TICKS), 0);
        assert_eq!(remaining_seconds(0, u32::MAX), 235_987_215);
    }

    #[test]
    fn countdown_is_monotonic_and_shows_every_second() {
        let mut previous = remaining_seconds(0, AUTO_BOOT_TICKS);
        let mut seen = [false; 6];
        for elapsed in 0..=AUTO_BOOT_TICKS {
            let now = remaining_seconds(elapsed, AUTO_BOOT_TICKS);
            assert!(now <= previous && previous - now <= 1);
            seen[now as usize] = true;
            previous = now;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn countdown_across_midnight() {
        let started = BIOS_TICKS_PER_DAY - 10;
        let elapsed = elapsed_ticks(started, 9);
        assert_eq!(elapsed, 19);
        assert_eq!(remaining_seconds(elapsed, AUTO_BOOT_TICKS), 4);
    }
}
