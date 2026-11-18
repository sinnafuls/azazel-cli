//! Who is signed in: pairing links, devices, and the limits on guessing.
//!
//! Only SHA-256 hashes of secrets and tokens are kept, in memory. Nothing is written to disk, so a
//! restart signs every device out.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::Access;
use crate::limits::{Clock, Window, since_epoch};

const MINUTE: Duration = Duration::from_secs(60);
/// Failed pairings one address may make a minute, and all addresses together.
const PER_ADDRESS: usize = 5;
const IN_ALL: usize = 20;
/// Failures in a row after which every link is void.
const VOID_AFTER: u32 = 10;

/// A signed-in browser or program.
#[derive(Debug, Clone)]
pub(crate) struct Device {
    /// Names it in counts and logs; not a secret.
    pub id: String,
    pub access: Access,
    pub paired: SystemTime,
    pub expires: SystemTime,
    /// Cancelled when the device is signed out, revoked or expires.
    pub cancel: CancellationToken,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PairError {
    /// The secret is unknown, was used, or has expired.
    Invalid,
    /// Too many failures; try later.
    TooMany,
}

struct PendingLink {
    access: Access,
    expires: SystemTime,
}

#[derive(Default)]
struct Inner {
    links: HashMap<String, PendingLink>,
    devices: HashMap<String, Device>,
    per_address: HashMap<IpAddr, Window>,
    all: Option<Window>,
    in_a_row: u32,
}

pub(crate) struct Auth {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
    link_ttl: Duration,
    device_ttl: Duration,
}

fn hash(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

fn random() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

impl Auth {
    pub fn new(clock: Arc<dyn Clock>, link_ttl: Duration, device_ttl: Duration) -> Auth {
        Auth {
            clock,
            inner: Mutex::new(Inner::default()),
            link_ttl,
            device_ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// A new single-use secret and when it stops working.
    pub fn mint_link(&self, access: Access) -> (String, SystemTime) {
        let secret = random();
        let expires = self.clock.now() + self.link_ttl;
        self.lock()
            .links
            .insert(hash(&secret), PendingLink { access, expires });
        (secret, expires)
    }

    /// Turns a pairing secret into a device and its token.
    pub fn pair(&self, secret: &str, from: IpAddr) -> Result<(String, Device), PairError> {
        let now = self.clock.now();
        let at = since_epoch(now);
        let mut inner = self.lock();
        inner.links.retain(|_, link| link.expires > now);
        let all = inner.all.get_or_insert_with(|| Window::new(MINUTE, IN_ALL));
        let all_open = all.open(at);
        let address_open = inner
            .per_address
            .entry(from)
            .or_insert_with(|| Window::new(MINUTE, PER_ADDRESS))
            .open(at);
        if !all_open || !address_open {
            return Err(PairError::TooMany);
        }
        let Some(link) = inner.links.remove(&hash(secret)) else {
            if let Some(all) = inner.all.as_mut() {
                all.record(at);
            }
            if let Some(window) = inner.per_address.get_mut(&from) {
                window.record(at);
            }
            inner.in_a_row += 1;
            if inner.in_a_row >= VOID_AFTER {
                inner.links.clear();
            }
            return Err(PairError::Invalid);
        };
        inner.in_a_row = 0;
        inner.per_address.retain(|_, window| !window.is_idle(at));
        let token = random();
        let digest = hash(&token);
        let device = Device {
            id: digest.chars().take(10).collect(),
            access: link.access,
            paired: now,
            expires: now + self.device_ttl,
            cancel: CancellationToken::new(),
        };
        inner.devices.insert(digest, device.clone());
        Ok((token, device))
    }

    /// The device a token belongs to, while it is signed in.
    pub fn device(&self, token: &str) -> Option<Device> {
        let now = self.clock.now();
        let digest = hash(token);
        let mut inner = self.lock();
        let device = inner.devices.get(&digest)?;
        if device.expires <= now {
            device.cancel.cancel();
            inner.devices.remove(&digest);
            return None;
        }
        Some(device.clone())
    }

    /// Every device that is signed in, the one paired first at the top. Tokens are not kept, so
    /// none can be given out.
    pub fn devices(&self) -> Vec<Device> {
        let now = self.clock.now();
        let mut devices: Vec<Device> = self
            .lock()
            .devices
            .values()
            .filter(|device| device.expires > now)
            .cloned()
            .collect();
        devices.sort_by_key(|device| device.paired);
        devices
    }

    pub fn sign_out(&self, token: &str) {
        if let Some(device) = self.lock().devices.remove(&hash(token)) {
            device.cancel.cancel();
        }
    }

    /// Signs every device out and voids every link.
    pub fn revoke_all(&self) {
        let mut inner = self.lock();
        for device in inner.devices.values() {
            device.cancel.cancel();
        }
        inner.devices.clear();
        inner.links.clear();
        inner.in_a_row = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::testing::FakeClock;

    const ME: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn auth() -> (Auth, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::default());
        let auth = Auth::new(
            clock.clone(),
            Duration::from_secs(15 * 60),
            Duration::from_secs(24 * 3600),
        );
        (auth, clock)
    }

    #[test]
    fn a_link_pairs_one_device_once() {
        let (auth, _) = auth();
        let (secret, _) = auth.mint_link(Access::Control);
        let (token, device) = auth.pair(&secret, ME).unwrap();
        assert_eq!(device.access, Access::Control);
        assert_eq!(auth.device(&token).unwrap().id, device.id);
        assert_eq!(auth.pair(&secret, ME).unwrap_err(), PairError::Invalid);
    }

    #[test]
    fn a_link_expires_and_so_does_a_device() {
        let (auth, clock) = auth();
        let (stale, _) = auth.mint_link(Access::View);
        clock.advance(Duration::from_secs(15 * 60));
        assert_eq!(auth.pair(&stale, ME).unwrap_err(), PairError::Invalid);
        let (secret, _) = auth.mint_link(Access::View);
        let (token, device) = auth.pair(&secret, ME).unwrap();
        clock.advance(Duration::from_secs(24 * 3600 - 1));
        assert!(auth.device(&token).is_some());
        clock.advance(Duration::from_secs(1));
        assert!(auth.device(&token).is_none());
        assert!(device.cancel.is_cancelled());
    }

    #[test]
    fn revoking_signs_every_device_out_and_voids_the_links() {
        let (auth, _) = auth();
        let (secret, _) = auth.mint_link(Access::Control);
        let (token, device) = auth.pair(&secret, ME).unwrap();
        let (open, _) = auth.mint_link(Access::View);
        auth.revoke_all();
        assert!(auth.device(&token).is_none() && device.cancel.is_cancelled());
        assert_eq!(auth.pair(&open, ME).unwrap_err(), PairError::Invalid);
    }

    #[test]
    fn five_failures_a_minute_stop_an_address_even_with_the_right_secret() {
        let (auth, clock) = auth();
        let (secret, _) = auth.mint_link(Access::Control);
        for _ in 0..5 {
            assert_eq!(auth.pair("wrong", ME).unwrap_err(), PairError::Invalid);
        }
        assert_eq!(auth.pair(&secret, ME).unwrap_err(), PairError::TooMany);
        let elsewhere = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        assert!(auth.pair(&secret, elsewhere).is_ok());
        clock.advance(MINUTE);
        assert_eq!(auth.pair("wrong", ME).unwrap_err(), PairError::Invalid);
    }

    #[test]
    fn twenty_failures_a_minute_from_anywhere_stop_everyone() {
        let (auth, _) = auth();
        for n in 0..20u8 {
            let from = IpAddr::V4(Ipv4Addr::new(10, 0, 1, n));
            assert_eq!(auth.pair("wrong", from).unwrap_err(), PairError::Invalid);
        }
        let (secret, _) = auth.mint_link(Access::Control);
        let late = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 1));
        assert_eq!(auth.pair(&secret, late).unwrap_err(), PairError::TooMany);
    }

    #[test]
    fn ten_failures_in_a_row_void_the_links_that_were_open() {
        let (auth, clock) = auth();
        let (secret, _) = auth.mint_link(Access::Control);
        for n in 0..10u8 {
            // Spread out, so that no rate limit is what stops the pairing.
            clock.advance(Duration::from_secs(30));
            let from = IpAddr::V4(Ipv4Addr::new(10, 0, 3, n));
            assert_eq!(auth.pair("wrong", from).unwrap_err(), PairError::Invalid);
        }
        clock.advance(MINUTE);
        assert_eq!(auth.pair(&secret, ME).unwrap_err(), PairError::Invalid);
        let (fresh, _) = auth.mint_link(Access::Control);
        assert!(auth.pair(&fresh, ME).is_ok());
    }
}
