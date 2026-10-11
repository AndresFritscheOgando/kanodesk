//! Monotonic microsecond clock shared by capture timestamps.
//!
//! On Windows this is `QueryPerformanceCounter`, which is system-wide, so two
//! processes on the same machine can compare timestamps (the phase 0 latency
//! overlay relies on this). Elsewhere it is process-local.

/// Microseconds on the capture timebase.
pub fn now_us() -> u64 {
    imp::now_us()
}

/// Converts `ticks` at `freq` ticks per second to microseconds without overflow.
pub fn ticks_to_us(ticks: u64, freq: u64) -> u64 {
    assert!(freq > 0, "zero clock frequency");
    let secs = ticks / freq;
    let rem = ticks % freq;
    secs * 1_000_000 + rem * 1_000_000 / freq
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    pub(crate) fn frequency() -> u64 {
        static FREQ: OnceLock<u64> = OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: valid out pointer; cannot fail on Windows XP and later.
            unsafe { QueryPerformanceFrequency(&mut f) }.expect("QueryPerformanceFrequency");
            f as u64
        })
    }

    pub(crate) fn now_us() -> u64 {
        let mut t = 0i64;
        // SAFETY: valid out pointer; cannot fail on Windows XP and later.
        unsafe { QueryPerformanceCounter(&mut t) }.expect("QueryPerformanceCounter");
        super::ticks_to_us(t as u64, frequency())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::OnceLock;
    use std::time::Instant;

    pub(crate) fn now_us() -> u64 {
        static BASE: OnceLock<Instant> = OnceLock::new();
        BASE.get_or_init(Instant::now).elapsed().as_micros() as u64
    }
}

/// Converts a raw `QueryPerformanceCounter` value (as found in DXGI frame
/// info) to microseconds on the capture timebase.
#[cfg(windows)]
pub fn qpc_to_us(ticks: i64) -> u64 {
    ticks_to_us(ticks.max(0) as u64, imp::frequency())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_to_us_exact_and_no_overflow() {
        assert_eq!(ticks_to_us(10_000_000, 10_000_000), 1_000_000);
        assert_eq!(ticks_to_us(15, 10), 1_500_000);
        // A huge tick count must not overflow the intermediate multiply.
        let big = u64::MAX / 2;
        assert_eq!(ticks_to_us(big, 1_000_000), big);
    }

    #[test]
    fn now_is_monotonic() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
    }
}
