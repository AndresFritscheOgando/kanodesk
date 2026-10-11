//! Login throttling (04 §7, 16 §5 item 7).
//!
//! Policy (all values in [`ThrottleConfig`]):
//!
//! - **Per peer** (by source IP; later also by device key):
//!   - the first `free_attempts` consecutive failures cost nothing; after
//!     `n ≥ free_attempts` of them the next attempt must wait
//!     `base_backoff × 2^(n − free_attempts)`, capped at `max_backoff`
//!     (defaults: 3 free, then 1 s, 2 s, 4 s … 300 s);
//!   - more than `window_max` failures within `window` locks the peer out
//!     until the oldest one leaves the window (RustDesk: about 6 per minute);
//!   - `ban_after` failures in total ban the peer until the host's owner
//!     resets the limits (RustDesk: about 30, until restart).
//! - **Global** (all peers together, against distributed guessing):
//!   - more than `global_window_max` failures within `window` makes every
//!     peer wait `global_lockout`;
//!   - `rotate_after` consecutive failures across all peers (reset by any
//!     success) replace the temporary password and the counter restarts. The
//!     permanent password is never rotated.
//!
//! Only completed login attempts count (see [`crate::login`]). Time is passed
//! in, so the policy is testable without sleeping.
//!
//! The state survives restarts: [`save`] writes it to a small text file
//! (wall-clock times) and [`load`] reads it back, so restarting a host does
//! not reset backoffs, windows or bans. Deleting the file resets them.

use std::collections::{HashMap, VecDeque};
use std::fmt::Display;
use std::hash::Hash;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::CryptoError;

/// Throttling parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// Consecutive failures per peer before backoff starts.
    pub free_attempts: u32,
    /// First backoff delay.
    pub base_backoff: Duration,
    /// Longest backoff delay.
    pub max_backoff: Duration,
    /// Sliding window for the rate limits.
    pub window: Duration,
    /// Failures per peer allowed within `window`.
    pub window_max: usize,
    /// Total failures after which a peer is banned (until the limits are reset).
    pub ban_after: u32,
    /// Failures from all peers allowed within `window`.
    pub global_window_max: usize,
    /// How long everyone waits once the global limit is hit.
    pub global_lockout: Duration,
    /// Consecutive failures (all peers) that rotate the temporary password.
    pub rotate_after: u32,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        ThrottleConfig {
            free_attempts: 3,
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            window: Duration::from_secs(60),
            window_max: 6,
            ban_after: 30,
            global_window_max: 60,
            global_lockout: Duration::from_secs(30),
            rotate_after: 10,
        }
    }
}

/// Why an attempt may not proceed now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Throttled {
    /// Try again after this long.
    RetryAfter(Duration),
    /// This peer is banned until the host's owner resets the limits.
    Banned,
}

/// What a recorded failure triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FailureOutcome {
    /// The temporary password should be replaced now.
    pub rotate_temporary: bool,
    /// This failure banned the peer.
    pub banned: bool,
}

#[derive(Debug, Default)]
struct PeerState {
    consecutive: u32,
    total: u32,
    recent: VecDeque<Instant>,
    last_failure: Option<Instant>,
}

/// Failure counters for one host.
#[derive(Debug)]
pub struct LoginThrottle<P> {
    config: ThrottleConfig,
    peers: HashMap<P, PeerState>,
    global_recent: VecDeque<Instant>,
    global_locked_until: Option<Instant>,
    global_consecutive: u32,
}

/// Delay required after `consecutive` failures.
fn backoff(config: &ThrottleConfig, consecutive: u32) -> Duration {
    if consecutive < config.free_attempts {
        return Duration::ZERO;
    }
    let factor = 1u32
        .checked_shl(consecutive - config.free_attempts)
        .unwrap_or(u32::MAX);
    config
        .base_backoff
        .saturating_mul(factor)
        .min(config.max_backoff)
}

fn prune(window: &mut VecDeque<Instant>, now: Instant, span: Duration) {
    while window
        .front()
        .is_some_and(|&t| now.saturating_duration_since(t) >= span)
    {
        window.pop_front();
    }
}

impl<P: Eq + Hash + Clone> LoginThrottle<P> {
    /// A throttle with the given policy.
    pub fn new(config: ThrottleConfig) -> Self {
        LoginThrottle {
            config,
            peers: HashMap::new(),
            global_recent: VecDeque::new(),
            global_locked_until: None,
            global_consecutive: 0,
        }
    }

    /// The policy.
    pub fn config(&self) -> &ThrottleConfig {
        &self.config
    }

    /// May `peer` attempt a login at `now`?
    pub fn check(&mut self, peer: &P, now: Instant) -> Result<(), Throttled> {
        let mut wait = Duration::ZERO;
        if let Some(until) = self.global_locked_until {
            wait = wait.max(until.saturating_duration_since(now));
        }
        let config = &self.config;
        if let Some(state) = self.peers.get_mut(peer) {
            if state.total >= config.ban_after {
                return Err(Throttled::Banned);
            }
            prune(&mut state.recent, now, config.window);
            if state.recent.len() >= config.window_max
                && let Some(&oldest) = state.recent.front()
            {
                wait = wait.max((oldest + config.window).saturating_duration_since(now));
            }
            if let Some(last) = state.last_failure {
                let delay = backoff(config, state.consecutive);
                wait = wait.max((last + delay).saturating_duration_since(now));
            }
        }
        if wait.is_zero() {
            Ok(())
        } else {
            Err(Throttled::RetryAfter(wait))
        }
    }

    /// Records a failed login.
    pub fn record_failure(&mut self, peer: &P, now: Instant) -> FailureOutcome {
        let config = self.config.clone();
        let state = self.peers.entry(peer.clone()).or_default();
        state.consecutive += 1;
        state.total += 1;
        state.last_failure = Some(now);
        prune(&mut state.recent, now, config.window);
        state.recent.push_back(now);
        let banned = state.total == config.ban_after;

        prune(&mut self.global_recent, now, config.window);
        self.global_recent.push_back(now);
        if self.global_recent.len() > config.global_window_max {
            self.global_locked_until = Some(now + config.global_lockout);
        }

        self.global_consecutive += 1;
        let rotate_temporary = self.global_consecutive >= config.rotate_after;
        if rotate_temporary {
            self.global_consecutive = 0;
        }
        FailureOutcome {
            rotate_temporary,
            banned,
        }
    }

    /// Records a successful login: the peer's consecutive count and the
    /// global consecutive count restart. Totals and windows are kept.
    pub fn record_success(&mut self, peer: &P) {
        if let Some(state) = self.peers.get_mut(peer) {
            state.consecutive = 0;
            state.last_failure = None;
        }
        self.global_consecutive = 0;
    }
}

/// First line of the saved state.
const FILE_HEADER: &str = "kanodesk-login-throttle 1";

/// Converts between `Instant`s and wall-clock milliseconds around one moment.
#[derive(Clone, Copy)]
struct Clock {
    now: Instant,
    wall_ms: u64,
}

impl Clock {
    fn new(now: Instant, wall: SystemTime) -> Self {
        let wall_ms = wall
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64);
        Clock { now, wall_ms }
    }

    fn to_ms(self, t: Instant) -> u64 {
        if t <= self.now {
            let ago = self.now.duration_since(t).as_millis() as u64;
            self.wall_ms.saturating_sub(ago)
        } else {
            let ahead = t.duration_since(self.now).as_millis() as u64;
            self.wall_ms.saturating_add(ahead)
        }
    }

    /// A past event. `None` if it is too far back to represent. Never after
    /// `now`, even if the wall clock went backwards since it was saved.
    fn past(self, ms: u64) -> Option<Instant> {
        self.now
            .checked_sub(Duration::from_millis(self.wall_ms.saturating_sub(ms)))
    }

    /// A future moment, at most `cap` from now.
    fn future(self, ms: u64, cap: Duration) -> Instant {
        self.now + Duration::from_millis(ms.saturating_sub(self.wall_ms)).min(cap)
    }
}

fn times(list: &VecDeque<Instant>, clock: Clock) -> String {
    if list.is_empty() {
        return "-".into();
    }
    let ms: Vec<String> = list.iter().map(|&t| clock.to_ms(t).to_string()).collect();
    ms.join(",")
}

fn parse_times(field: &str, clock: Clock) -> Option<VecDeque<Instant>> {
    if field == "-" {
        return Some(VecDeque::new());
    }
    let mut out = Vec::new();
    for ms in field.split(',') {
        if let Some(t) = clock.past(ms.parse().ok()?) {
            out.push(t);
        }
    }
    out.sort();
    Some(out.into())
}

fn opt_time(t: Option<Instant>, clock: Clock) -> String {
    t.map_or_else(|| "-".into(), |t| clock.to_ms(t).to_string())
}

impl<P: Eq + Hash + Clone + Display + FromStr> LoginThrottle<P> {
    /// The state as text, with times as wall-clock milliseconds
    /// (`now` and `wall` are the same moment on the two clocks).
    pub fn to_text(&self, now: Instant, wall: SystemTime) -> String {
        let clock = Clock::new(now, wall);
        let mut out = format!("{FILE_HEADER}\n");
        out += &format!(
            "global {} {} {}\n",
            self.global_consecutive,
            opt_time(self.global_locked_until, clock),
            times(&self.global_recent, clock)
        );
        for (peer, state) in &self.peers {
            out += &format!(
                "peer {peer} {} {} {} {}\n",
                state.consecutive,
                state.total,
                opt_time(state.last_failure, clock),
                times(&state.recent, clock)
            );
        }
        out
    }

    /// Restores state saved by [`Self::to_text`]. Lines that do not parse
    /// are skipped; text in another format gives a fresh throttle.
    pub fn from_text(config: ThrottleConfig, text: &str, now: Instant, wall: SystemTime) -> Self {
        let clock = Clock::new(now, wall);
        let mut throttle = LoginThrottle::new(config);
        let mut lines = text.lines();
        if lines.next() != Some(FILE_HEADER) {
            return throttle;
        }
        for line in lines {
            let fields: Vec<&str> = line.split(' ').collect();
            match fields.as_slice() {
                ["global", consecutive, locked, recent] => {
                    let (Ok(consecutive), Some(recent)) =
                        (consecutive.parse(), parse_times(recent, clock))
                    else {
                        continue;
                    };
                    throttle.global_consecutive = consecutive;
                    throttle.global_recent = recent;
                    let cap = throttle.config.global_lockout;
                    throttle.global_locked_until =
                        locked.parse().ok().map(|ms| clock.future(ms, cap));
                }
                ["peer", peer, consecutive, total, last, recent] => {
                    let (Ok(peer), Ok(consecutive), Ok(total), Some(recent)) = (
                        peer.parse::<P>(),
                        consecutive.parse(),
                        total.parse(),
                        parse_times(recent, clock),
                    ) else {
                        continue;
                    };
                    let last_failure = last.parse().ok().and_then(|ms| clock.past(ms));
                    throttle.peers.insert(
                        peer,
                        PeerState {
                            consecutive,
                            total,
                            recent,
                            last_failure,
                        },
                    );
                }
                _ => {}
            }
        }
        throttle
    }
}

/// Loads the throttle state from `path`; a missing or unreadable file gives
/// a fresh throttle.
pub fn load<P: Eq + Hash + Clone + Display + FromStr>(
    path: &Path,
    config: ThrottleConfig,
) -> LoginThrottle<P> {
    match std::fs::read_to_string(path) {
        Ok(text) => LoginThrottle::from_text(config, &text, Instant::now(), SystemTime::now()),
        Err(_) => LoginThrottle::new(config),
    }
}

/// Saves the throttle state to `path` (written to a temporary file, then
/// renamed, so a crash never leaves half a file).
pub fn save<P: Eq + Hash + Clone + Display + FromStr>(
    path: &Path,
    throttle: &LoginThrottle<P>,
) -> Result<(), CryptoError> {
    let text = throttle.to_text(Instant::now(), SystemTime::now());
    let tmp = path.with_extension("tmp");
    let io = |e: std::io::Error| CryptoError::KeyStore(format!("{}: {e}", path.display()));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn throttle() -> LoginThrottle<&'static str> {
        LoginThrottle::new(ThrottleConfig::default())
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn first_attempts_are_free_then_backoff_doubles() {
        let mut t = throttle();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(t.check(&"a", t0).is_ok());
            t.record_failure(&"a", t0);
        }
        // 3 consecutive failures: 1 s, then 2 s, 4 s ...
        assert_eq!(t.check(&"a", t0), Err(Throttled::RetryAfter(secs(1))));
        assert!(t.check(&"a", t0 + secs(1)).is_ok());
        t.record_failure(&"a", t0 + secs(1));
        assert_eq!(
            t.check(&"a", t0 + secs(1)),
            Err(Throttled::RetryAfter(secs(2)))
        );
        t.record_failure(&"a", t0 + secs(3));
        assert_eq!(
            t.check(&"a", t0 + secs(3)),
            Err(Throttled::RetryAfter(secs(4)))
        );
        // Other peers are unaffected.
        assert!(t.check(&"b", t0 + secs(3)).is_ok());
    }

    #[test]
    fn backoff_is_capped() {
        let c = ThrottleConfig::default();
        assert_eq!(backoff(&c, 2), Duration::ZERO);
        assert_eq!(backoff(&c, 3), secs(1));
        assert_eq!(backoff(&c, 4), secs(2));
        assert_eq!(backoff(&c, 11), secs(256));
        assert_eq!(backoff(&c, 12), secs(300));
        assert_eq!(backoff(&c, 1000), secs(300));
    }

    #[test]
    fn window_limit_locks_until_the_oldest_failure_expires() {
        let config = ThrottleConfig {
            free_attempts: 100,
            ..ThrottleConfig::default()
        };
        let mut t = LoginThrottle::new(config);
        let t0 = Instant::now();
        for i in 0..6 {
            t.record_failure(&"a", t0 + secs(i));
        }
        assert_eq!(
            t.check(&"a", t0 + secs(10)),
            Err(Throttled::RetryAfter(secs(50)))
        );
        assert!(t.check(&"a", t0 + secs(60)).is_ok());
    }

    #[test]
    fn success_resets_the_backoff() {
        let mut t = throttle();
        let t0 = Instant::now();
        for _ in 0..5 {
            t.record_failure(&"a", t0);
        }
        assert!(t.check(&"a", t0).is_err());
        t.record_success(&"a");
        // Window limit (6/min) not reached, backoff gone.
        assert!(t.check(&"a", t0).is_ok());
    }

    #[test]
    fn total_failures_ban_the_peer() {
        let mut t = throttle();
        let mut now = Instant::now();
        let mut banned_at = None;
        for i in 0..30 {
            now += secs(3600); // spread out so only the total matters
            if t.record_failure(&"a", now).banned {
                banned_at = Some(i + 1);
            }
            t.record_success(&"a"); // even interleaved successes do not reset the total
        }
        assert_eq!(banned_at, Some(30));
        assert_eq!(t.check(&"a", now + secs(100_000)), Err(Throttled::Banned));
    }

    #[test]
    fn many_peers_trigger_the_global_lockout() {
        let mut t = LoginThrottle::new(ThrottleConfig::default());
        let t0 = Instant::now();
        for i in 0..61u32 {
            t.record_failure(&i, t0);
        }
        // A fresh peer must wait too.
        assert_eq!(t.check(&999, t0), Err(Throttled::RetryAfter(secs(30))));
        assert!(t.check(&999, t0 + secs(30)).is_ok());
    }

    fn restore<P: Eq + Hash + Clone + Display + FromStr>(
        text: &str,
        now: Instant,
        wall: SystemTime,
    ) -> LoginThrottle<P> {
        LoginThrottle::from_text(ThrottleConfig::default(), text, now, wall)
    }

    #[test]
    fn state_survives_a_restart() {
        let t0 = Instant::now();
        let wall = SystemTime::now();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "2001:db8::2".parse().unwrap();
        let mut t: LoginThrottle<IpAddr> = LoginThrottle::new(ThrottleConfig::default());
        for _ in 0..4 {
            t.record_failure(&a, t0);
        }
        for i in 0..30 {
            t.record_failure(&b, t0 + secs(i * 100));
        }
        // Saved 3000 s after a's failures; restored 1 s later on a new Instant base.
        let text = t.to_text(t0 + secs(3000), wall);
        let later = Instant::now() + secs(10);
        let mut r: LoginThrottle<IpAddr> = restore(&text, later, wall + secs(1));
        assert_eq!(r.check(&b, later), Err(Throttled::Banned), "bans persist");
        // a's backoff is long over, but its count is kept: the next failure
        // (the 5th in a row) means waiting 4 s.
        assert!(r.check(&a, later).is_ok());
        r.record_failure(&a, later);
        assert_eq!(r.check(&a, later), Err(Throttled::RetryAfter(secs(4))));

        // A backoff that was still running keeps running after the restart.
        let mut t: LoginThrottle<IpAddr> = LoginThrottle::new(ThrottleConfig::default());
        for _ in 0..4 {
            t.record_failure(&a, t0);
        }
        let text = t.to_text(t0, wall);
        let mut r: LoginThrottle<IpAddr> = restore(&text, later, wall + secs(1));
        assert_eq!(r.check(&a, later), Err(Throttled::RetryAfter(secs(1))));
    }

    #[test]
    fn global_lockout_and_rotation_count_are_restored() {
        let t0 = Instant::now();
        let wall = SystemTime::now();
        let mut t: LoginThrottle<u32> = LoginThrottle::new(ThrottleConfig::default());
        for i in 0..61u32 {
            t.record_failure(&i, t0);
        }
        let text = t.to_text(t0, wall);
        let later = Instant::now();
        let mut r: LoginThrottle<u32> = restore(&text, later, wall + secs(10));
        // 30 s lockout, 10 s of it gone.
        assert_eq!(r.check(&999, later), Err(Throttled::RetryAfter(secs(20))));
        // 61 failures in a row rotated at 10, 20 ... 60; one is counted towards the next.
        assert_eq!(r.global_consecutive, 1);
    }

    #[test]
    fn bad_saved_state_gives_a_fresh_throttle() {
        let now = Instant::now();
        let wall = SystemTime::now();
        for text in [
            "",
            "something else\npeer 1 9 9 - -",
            "kanodesk-login-throttle 1\npeer x y z\nglobal 1\npeer 1 a 1 - -",
        ] {
            let mut r: LoginThrottle<u32> = restore(text, now, wall);
            assert!(r.peers.is_empty(), "{text:?}");
            assert!(r.check(&1, now).is_ok());
        }
        // A wall clock that went backwards does not put failures in the future.
        let mut t: LoginThrottle<u32> = LoginThrottle::new(ThrottleConfig::default());
        t.record_failure(&1, now);
        let text = t.to_text(now, wall);
        let r: LoginThrottle<u32> = restore(&text, now, wall - secs(3600));
        assert!(r.peers[&1].recent.iter().all(|&t| t <= now));
    }

    #[test]
    fn save_and_load_use_a_file() {
        let dir = std::env::temp_dir().join(format!("kd-throttle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("login-throttle");
        let fresh: LoginThrottle<u32> = load(&path, ThrottleConfig::default());
        assert!(fresh.peers.is_empty());
        let mut t: LoginThrottle<u32> = LoginThrottle::new(ThrottleConfig::default());
        for _ in 0..30 {
            t.record_failure(&7, Instant::now());
        }
        save(&path, &t).unwrap();
        let mut back: LoginThrottle<u32> = load(&path, ThrottleConfig::default());
        assert_eq!(back.check(&7, Instant::now()), Err(Throttled::Banned));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn consecutive_failures_across_peers_rotate_the_temporary_password() {
        let mut t = LoginThrottle::new(ThrottleConfig::default());
        let t0 = Instant::now();
        let mut rotations = 0;
        for i in 0..25u32 {
            if t.record_failure(&i, t0).rotate_temporary {
                rotations += 1;
            }
            if i == 4 {
                // A success in between restarts the count.
                t.record_success(&i);
            }
        }
        // Failures 0-4, then reset; 5-14 rotate once, 15-24 rotate again.
        assert_eq!(rotations, 2);
    }
}
