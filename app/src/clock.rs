//! One clock for both targets. `std::time::Instant::now` panics on wasm32
//! ("time not implemented on this platform"), so the target-neutral driver
//! reads time through [`Instant`]: milliseconds from `Date.now()` on the web,
//! and from a process-start anchor on native.

use std::ops::Add;
use std::time::Duration;

/// A point in time, in milliseconds since an arbitrary origin.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Instant(f64);

impl Instant {
    pub fn now() -> Self {
        Self(now_ms())
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, rhs: Duration) -> Instant {
        Instant(self.0 + rhs.as_secs_f64() * 1000.0)
    }
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    js_sys::Date::now()
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    use std::sync::OnceLock;
    static ANCHOR: OnceLock<std::time::Instant> = OnceLock::new();
    ANCHOR.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_a_duration_moves_the_instant_forward() {
        let start = Instant::now();
        let later = start + Duration::from_millis(1500);
        assert!(later >= start + Duration::from_millis(1499));
        assert!(later > start);
    }

    #[test]
    fn the_clock_does_not_go_backwards() {
        let a = Instant::now();
        let b = Instant::now();
        assert!(b >= a);
    }
}
