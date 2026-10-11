//! Device IDs: 9 decimal digits, first digit non-zero (100 000 000 –
//! 999 999 999), assigned at random by the ID server on first registration.

use std::fmt;
use std::str::FromStr;

/// Smallest valid ID.
pub const MIN: u32 = 100_000_000;
/// Largest valid ID.
pub const MAX: u32 = 999_999_999;
const SPAN: u32 = MAX - MIN + 1;

/// A 9-digit device ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(u32);

/// The text is not a 9-digit ID.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("device IDs are 9 digits, not starting with 0")]
pub struct InvalidId;

impl DeviceId {
    /// Wraps a number in range.
    pub fn new(value: u32) -> Result<Self, InvalidId> {
        if (MIN..=MAX).contains(&value) {
            Ok(DeviceId(value))
        } else {
            Err(InvalidId)
        }
    }

    /// The numeric value.
    pub fn get(self) -> u32 {
        self.0
    }

    /// A uniformly random ID from `fill` (rejection sampling, no modulo bias).
    pub fn random_with(
        mut fill: impl FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> Result<Self, getrandom::Error> {
        // Largest multiple of SPAN below 2^32.
        let limit = u32::MAX - (u32::MAX % SPAN) - 1;
        loop {
            let mut b = [0u8; 4];
            fill(&mut b)?;
            let v = u32::from_le_bytes(b);
            if v <= limit {
                return Ok(DeviceId(MIN + v % SPAN));
            }
        }
    }

    /// A uniformly random ID from the OS random number generator.
    pub fn random() -> Result<Self, getrandom::Error> {
        Self::random_with(getrandom::fill)
    }

    /// Grouped for people to read and type: `123 456 789`.
    pub fn grouped(self) -> String {
        let s = self.0.to_string();
        format!("{} {} {}", &s[0..3], &s[3..6], &s[6..9])
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.0)
    }
}

impl FromStr for DeviceId {
    type Err = InvalidId;

    /// Accepts 9 digits, ignoring spaces and dashes between groups.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits: String = s.chars().filter(|c| *c != ' ' && *c != '-').collect();
        if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InvalidId);
        }
        DeviceId::new(digits.parse().map_err(|_| InvalidId)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints() {
        let id: DeviceId = "123456789".parse().unwrap();
        assert_eq!(id.to_string(), "123456789");
        assert_eq!(id.grouped(), "123 456 789");
        assert_eq!("123 456 789".parse::<DeviceId>().unwrap(), id);
        assert_eq!("123-456-789".parse::<DeviceId>().unwrap(), id);
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in [
            "",
            "12345678",
            "1234567890",
            "012345678",
            "12345678a",
            "+12345678",
            "１２３４５６７８９",
        ] {
            assert!(bad.parse::<DeviceId>().is_err(), "{bad:?}");
        }
        assert!(DeviceId::new(99_999_999).is_err());
        assert!(DeviceId::new(1_000_000_000).is_err());
    }

    #[test]
    fn random_ids_are_in_range_and_vary() {
        let ids: Vec<_> = (0..1000).map(|_| DeviceId::random().unwrap()).collect();
        assert!(ids.iter().all(|id| (MIN..=MAX).contains(&id.get())));
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert!(unique.len() > 990);
    }

    #[test]
    fn rejection_sampling_skips_the_biased_tail() {
        let mut calls = 0;
        let id = DeviceId::random_with(|b| {
            calls += 1;
            b.copy_from_slice(&if calls == 1 { u32::MAX } else { 5 }.to_le_bytes());
            Ok(())
        })
        .unwrap();
        assert_eq!(id.get(), MIN + 5);
        assert_eq!(calls, 2);
    }
}
