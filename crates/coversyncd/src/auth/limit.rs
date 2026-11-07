//! Failed logins, limited per client and overall, in memory.
//!
//! Each attempt takes a token from its client's bucket and from the overall
//! one before the password is checked, and a successful one gives them back.
//! Taking first also bounds how many password checks can run at once.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// A token bucket's size, and how fast it refills.
#[derive(Clone, Copy)]
struct Rate {
    burst: f64,
    per_second: f64,
}

/// Five tries from one client, then one a minute.
const CLIENT: Rate = Rate {
    burst: 5.0,
    per_second: 1.0 / 60.0,
};
/// Twenty tries from anywhere, then one every six seconds.
const OVERALL: Rate = Rate {
    burst: 20.0,
    per_second: 1.0 / 6.0,
};
/// Clients remembered before full buckets are dropped.
const PRUNE_AT: usize = 1024;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Self {
        Bucket {
            tokens: rate.burst,
            at: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate.per_second).min(rate.burst);
        self.at = now;
    }

    /// How long until a token is free; zero if one is.
    fn wait(&self, rate: Rate) -> Duration {
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / rate.per_second)
        }
    }
}

/// Who an attempt counts against: an IPv4 address, or an IPv6 /64, which
/// one host can easily have to itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Client(Option<IpAddr>);

impl Client {
    pub fn new(ip: Option<IpAddr>) -> Self {
        Client(ip.map(|ip| match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let mut octets = v6.octets();
                octets[8..].fill(0);
                IpAddr::from(octets)
            }
            v4 => v4,
        }))
    }
}

pub struct Limiter {
    clients: HashMap<Client, Bucket>,
    overall: Bucket,
}

impl Limiter {
    pub fn new(now: Instant) -> Self {
        Limiter {
            clients: HashMap::new(),
            overall: Bucket::full(OVERALL, now),
        }
    }

    /// Take an attempt for `client`, or say how long until it may try again.
    pub fn take(&mut self, client: Client, now: Instant) -> Result<(), Duration> {
        if self.clients.len() >= PRUNE_AT {
            self.clients.retain(|_, b| {
                b.refill(CLIENT, now);
                b.tokens < CLIENT.burst
            });
        }
        let mine = self
            .clients
            .entry(client)
            .or_insert_with(|| Bucket::full(CLIENT, now));
        mine.refill(CLIENT, now);
        self.overall.refill(OVERALL, now);
        let wait = mine.wait(CLIENT).max(self.overall.wait(OVERALL));
        if !wait.is_zero() {
            return Err(wait);
        }
        mine.tokens -= 1.0;
        self.overall.tokens -= 1.0;
        Ok(())
    }

    /// Give back an attempt that succeeded.
    pub fn refund(&mut self, client: Client, now: Instant) {
        if let Some(mine) = self.clients.get_mut(&client) {
            mine.refill(CLIENT, now);
            mine.tokens = (mine.tokens + 1.0).min(CLIENT.burst);
        }
        self.overall.refill(OVERALL, now);
        self.overall.tokens = (self.overall.tokens + 1.0).min(OVERALL.burst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Client {
        Client::new(Some(s.parse().unwrap()))
    }

    #[test]
    fn a_client_gets_five_tries_then_one_a_minute() {
        let t0 = Instant::now();
        let mut l = Limiter::new(t0);
        let me = ip("192.168.1.20");
        for _ in 0..5 {
            assert!(l.take(me, t0).is_ok());
        }
        let wait = l.take(me, t0).unwrap_err();
        assert!(
            wait > Duration::from_secs(59) && wait <= Duration::from_secs(60),
            "{wait:?}"
        );
        // Someone else isn't held up.
        assert!(l.take(ip("192.168.1.21"), t0).is_ok());
        assert!(l.take(me, t0 + Duration::from_secs(30)).is_err());
        assert!(l.take(me, t0 + Duration::from_secs(60)).is_ok());
        assert!(l.take(me, t0 + Duration::from_secs(61)).is_err());
    }

    #[test]
    fn successful_logins_give_their_try_back() {
        let t0 = Instant::now();
        let mut l = Limiter::new(t0);
        let me = ip("10.0.0.2");
        for _ in 0..20 {
            l.take(me, t0).unwrap();
            l.refund(me, t0);
        }
        assert!(l.take(me, t0).is_ok());
    }

    #[test]
    fn many_clients_together_are_limited_too() {
        let t0 = Instant::now();
        let mut l = Limiter::new(t0);
        for i in 0..20 {
            assert!(l.take(ip(&format!("10.0.1.{i}")), t0).is_ok());
        }
        let wait = l.take(ip("10.0.2.1"), t0).unwrap_err();
        assert!(wait <= Duration::from_secs(6), "{wait:?}");
        assert!(l.take(ip("10.0.2.1"), t0 + Duration::from_secs(6)).is_ok());
    }

    #[test]
    fn an_ipv6_host_counts_as_its_slash_64() {
        assert_eq!(ip("2001:db8:1:2::1"), ip("2001:db8:1:2:ffff::9"));
        assert_ne!(ip("2001:db8:1:2::1"), ip("2001:db8:1:3::1"));
        assert_eq!(ip("::ffff:192.168.1.5"), ip("192.168.1.5"));
    }

    #[test]
    fn idle_clients_are_forgotten() {
        let t0 = Instant::now();
        let mut l = Limiter::new(t0);
        for i in 0..PRUNE_AT {
            let c = Client::new(Some(IpAddr::from([
                10,
                1,
                (i / 256) as u8,
                (i % 256) as u8,
            ])));
            l.clients.insert(c, Bucket::full(CLIENT, t0));
        }
        l.take(ip("10.9.9.9"), t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(l.clients.len(), 1);
    }
}
