// INT 1Ah/AH=00h counts ticks since midnight, not across a u32 rollover.
pub const BIOS_TICKS_PER_DAY: u32 = 0x0018_00b0;

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
}
