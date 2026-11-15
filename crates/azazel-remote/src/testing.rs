//! Stand-ins for the tests of this crate.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::limits::Clock;

/// A clock the test moves by hand.
pub(crate) struct FakeClock(Mutex<SystemTime>);

impl Default for FakeClock {
    fn default() -> FakeClock {
        FakeClock(Mutex::new(UNIX_EPOCH + Duration::from_secs(1_800_000_000)))
    }
}

impl FakeClock {
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
