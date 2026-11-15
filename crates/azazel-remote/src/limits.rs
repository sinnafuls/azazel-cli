//! Counting: the clock the rest of the crate asks for the time, a window of recent events, a
//! token bucket for commands, and the number of open sockets.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the crate takes the time from, so that a test controls it.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> SystemTime;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Time as the span since the epoch, which windows and buckets count in.
pub(crate) fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}

/// At most `max` events within `span`.
#[derive(Debug)]
pub(crate) struct Window {
    span: Duration,
    max: usize,
    hits: VecDeque<Duration>,
}

impl Window {
    pub fn new(span: Duration, max: usize) -> Window {
        Window {
            span,
            max,
            hits: VecDeque::new(),
        }
    }

    fn trim(&mut self, now: Duration) {
        while self
            .hits
            .front()
            .is_some_and(|hit| now.saturating_sub(*hit) >= self.span)
        {
            self.hits.pop_front();
        }
    }

    /// Whether another event may happen now.
    pub fn open(&mut self, now: Duration) -> bool {
        self.trim(now);
        self.hits.len() < self.max
    }

    pub fn record(&mut self, now: Duration) {
        self.trim(now);
        self.hits.push_back(now);
    }

    pub fn is_idle(&mut self, now: Duration) -> bool {
        self.trim(now);
        self.hits.is_empty()
    }
}

/// `rate` events a second, with room for `burst` at once.
#[derive(Debug)]
pub(crate) struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Duration,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64, now: Duration) -> Bucket {
        Bucket {
            rate,
            burst,
            tokens: burst,
            at: now,
        }
    }

    /// Takes one token, or says there is none.
    pub fn take(&mut self, now: Duration) -> bool {
        let passed = now.saturating_sub(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + passed * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// How many WebSockets are open, in all and per device.
#[derive(Debug, Default)]
pub(crate) struct Connections {
    per_device: HashMap<String, usize>,
    total: usize,
}

pub(crate) const PER_DEVICE: usize = 4;
pub(crate) const IN_ALL: usize = 16;

/// Holds one place among the open sockets until it is dropped.
pub(crate) struct Permit {
    connections: Arc<Mutex<Connections>>,
    device: String,
}

impl Connections {
    pub fn acquire(connections: &Arc<Mutex<Connections>>, device: &str) -> Option<Permit> {
        let mut inner = connections.lock().unwrap_or_else(|e| e.into_inner());
        let count = inner.per_device.get(device).copied().unwrap_or(0);
        if count >= PER_DEVICE || inner.total >= IN_ALL {
            return None;
        }
        inner.per_device.insert(device.to_string(), count + 1);
        inner.total += 1;
        Some(Permit {
            connections: connections.clone(),
            device: device.to_string(),
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut inner = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        inner.total = inner.total.saturating_sub(1);
        if let Some(count) = inner.per_device.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                inner.per_device.remove(&self.device);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_window_forgets_events_older_than_its_span() {
        let mut window = Window::new(secs(60), 2);
        window.record(secs(0));
        window.record(secs(10));
        assert!(!window.open(secs(30)));
        assert!(window.open(secs(60)));
        assert!(window.is_idle(secs(71)));
    }

    #[test]
    fn a_bucket_allows_a_burst_then_the_rate() {
        let mut bucket = Bucket::new(20.0, 40.0, secs(0));
        assert_eq!((0..40).filter(|_| bucket.take(secs(0))).count(), 40);
        assert!(!bucket.take(secs(0)));
        // A tenth of a second brings two tokens.
        let later = Duration::from_millis(100);
        assert!(bucket.take(later) && bucket.take(later) && !bucket.take(later));
    }

    #[test]
    fn sockets_are_counted_per_device_and_in_all() {
        let connections = Arc::new(Mutex::new(Connections::default()));
        let held: Vec<_> = (0..PER_DEVICE)
            .map(|_| Connections::acquire(&connections, "a").expect("room"))
            .collect();
        assert!(Connections::acquire(&connections, "a").is_none());
        let others: Vec<_> = (0..IN_ALL - PER_DEVICE)
            .map(|n| Connections::acquire(&connections, &format!("d{}", n / PER_DEVICE)).expect("room"))
            .collect();
        assert!(Connections::acquire(&connections, "z").is_none());
        drop(held);
        assert!(Connections::acquire(&connections, "z").is_some());
        drop(others);
    }
}
