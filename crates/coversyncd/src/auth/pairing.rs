//! Pairing codes: what a logged-in browser's Add device makes, and a device
//! trades for its token at `POST /api/pair`.
//!
//! A code is 8 characters of Crockford base32 (40 random bits), works once,
//! and expires after 10 minutes. They are kept in memory only: a restart
//! cancels them, as closing the dialog does. After [`FAILURES`] attempts
//! that paired nothing, every waiting code is cancelled, on top of the rate
//! limits on the route itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::store::Scope;

/// How long a code works.
pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How long a code that has ended is remembered after its lifetime, so the
/// dialog can say what happened and a device trying it is told why.
const LINGER: Duration = Duration::from_secs(10 * 60);
/// Failed attempts that cancel every waiting code.
pub const FAILURES: u32 = 10;
/// Codes waiting at once (each open Add device dialog has one).
const MAX_WAITING: usize = 16;

/// Crockford's base32: digits and letters without I, L, O and U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Why a code was cancelled before it was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cancelled {
    /// Too many wrong codes were tried.
    Failures,
    /// It was sent over plain http, so it may have been seen on the way.
    Insecure,
    /// The device it would replace was removed.
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Waiting,
    /// Taken by a device, whose token is being issued.
    Claimed,
    Paired(i64),
    Cancelled(Cancelled),
    /// Its token couldn't be issued.
    Failed,
}

struct Pairing {
    code: String,
    name: String,
    scope: Scope,
    replace: Option<i64>,
    made: Instant,
    state: State,
}

impl Pairing {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.made) >= LIFETIME
    }

    fn waiting(&self, now: Instant) -> bool {
        self.state == State::Waiting && !self.expired(now)
    }
}

/// A code, as Add device shows it while the device pairs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Waiting {
        /// Seconds left.
        expires_in: u64,
    },
    /// A device redeemed it, and its token is being issued.
    Pairing,
    /// A device redeemed it.
    Paired {
        device: i64,
    },
    Expired,
    Cancelled {
        why: Cancelled,
    },
    Failed,
}

/// What a device that redeemed a code gets its token for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub replace: Option<i64>,
}

/// Why a code was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No such code.
    Wrong,
    /// It expired, or was cancelled.
    Expired,
    /// A device has used it already.
    Used,
}

#[derive(Default)]
pub struct Pairings {
    /// By id, which only the browser that made the code knows.
    codes: HashMap<String, Pairing>,
    /// Attempts that paired nothing since the oldest waiting code was made.
    failures: u32,
}

impl Pairings {
    /// Forget codes that ended a while ago.
    fn prune(&mut self, now: Instant) {
        self.codes
            .retain(|_, p| now.saturating_duration_since(p.made) < LIFETIME + LINGER);
    }

    /// Make a code for a device to pair as `name` with `scope`, taking over
    /// `replace`'s entry if given. Returns the pairing's id and the code, or
    /// `None` when too many are waiting.
    pub fn create(
        &mut self,
        name: String,
        scope: Scope,
        replace: Option<i64>,
        now: Instant,
    ) -> rusqlite::Result<Option<(String, String)>> {
        self.prune(now);
        let waiting = self.codes.values().filter(|p| p.waiting(now)).count();
        if waiting >= MAX_WAITING {
            return Ok(None);
        }
        // Failures before every waiting code count against none of them.
        if waiting == 0 {
            self.failures = 0;
        }
        let code = loop {
            let code = new_code()?;
            if !self.codes.values().any(|p| p.code == code) {
                break code;
            }
        };
        let id = super::store::random_secret()?;
        self.codes.insert(
            id.clone(),
            Pairing {
                code: code.clone(),
                name,
                scope,
                replace,
                made: now,
                state: State::Waiting,
            },
        );
        Ok(Some((id, code)))
    }

    /// Take a waiting code for a device: the code is used up from here on.
    /// `code` is already [`normalize`]d. Anything else counts as a failure.
    pub fn redeem(&mut self, code: &str, now: Instant) -> Result<Claim, Refused> {
        self.prune(now);
        let found = self.codes.iter_mut().find(|(_, p)| p.code == code);
        let refused = match found {
            Some((id, p)) if p.waiting(now) => {
                p.state = State::Claimed;
                return Ok(Claim {
                    id: id.clone(),
                    name: p.name.clone(),
                    scope: p.scope,
                    replace: p.replace,
                });
            }
            Some((_, p)) => match p.state {
                State::Claimed | State::Paired(_) | State::Failed => Refused::Used,
                State::Waiting | State::Cancelled(_) => Refused::Expired,
            },
            None => Refused::Wrong,
        };
        // A code that was used was right: a device trying it again (its
        // answer lost on the way, say) isn't guessing.
        if refused == Refused::Used {
            return Err(refused);
        }
        self.failures += 1;
        if self.failures >= FAILURES {
            self.failures = 0;
            for p in self.codes.values_mut() {
                if p.waiting(now) {
                    p.state = State::Cancelled(Cancelled::Failures);
                }
            }
        }
        Err(refused)
    }

    /// The device that claimed `id` has its token.
    pub fn paired(&mut self, id: &str, device: i64) {
        if let Some(p) = self.codes.get_mut(id) {
            p.state = State::Paired(device);
        }
    }

    /// The token for the device that claimed `id` couldn't be issued.
    pub fn failed(&mut self, id: &str) {
        if let Some(p) = self.codes.get_mut(id) {
            p.state = State::Failed;
        }
    }

    /// The device that claimed `id` gets no token: the device it would
    /// replace was removed while it paired.
    pub fn replaced_gone(&mut self, id: &str) {
        if let Some(p) = self.codes.get_mut(id) {
            p.state = State::Cancelled(Cancelled::Removed);
        }
    }

    /// Cancel the waiting codes among `codes`, sent where they may have been
    /// seen. `codes` is sorted: each waiting code is looked up in it once,
    /// however many there are.
    pub fn cancel_codes(&mut self, codes: &[String], why: Cancelled, now: Instant) {
        for p in self.codes.values_mut() {
            if p.waiting(now) && codes.binary_search(&p.code).is_ok() {
                p.state = State::Cancelled(why);
            }
        }
    }

    /// Cancel the waiting codes that would replace `device`, which is gone.
    pub fn forget_device(&mut self, device: i64) {
        for p in self.codes.values_mut() {
            if p.replace == Some(device) && p.state == State::Waiting {
                p.state = State::Cancelled(Cancelled::Removed);
            }
        }
    }

    pub fn status(&mut self, id: &str, now: Instant) -> Option<Status> {
        self.prune(now);
        let p = self.codes.get(id)?;
        Some(match p.state {
            State::Waiting if p.expired(now) => Status::Expired,
            State::Waiting => Status::Waiting {
                expires_in: (LIFETIME - now.saturating_duration_since(p.made)).as_secs(),
            },
            State::Claimed => Status::Pairing,
            State::Paired(device) => Status::Paired { device },
            State::Cancelled(why) => Status::Cancelled { why },
            State::Failed => Status::Failed,
        })
    }

    /// The dialog closed: a code still waiting stops working.
    pub fn close(&mut self, id: &str) {
        self.codes.remove(id);
    }
}

/// 40 random bits as 8 characters of Crockford base32.
fn new_code() -> rusqlite::Result<String> {
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let bits = bytes.iter().fold(0u64, |n, &b| n << 8 | u64::from(b));
    Ok((0..8)
        .map(|i| ALPHABET[(bits >> (35 - 5 * i) & 31) as usize] as char)
        .collect())
}

/// A code as typed: any case, with or without its hyphen and spaces, and
/// with O for 0 and I or L for 1, as Crockford's base32 allows. `None` if it
/// can't be one.
pub fn normalize(typed: &str) -> Option<String> {
    let mut code = String::with_capacity(8);
    for c in typed.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) || code.len() == 8 {
            return None;
        }
        code.push(c);
    }
    (code.len() == 8).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(p: &mut Pairings, now: Instant) -> (String, String) {
        p.create("Phone".into(), Scope::Manage, None, now)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn codes_are_crockford_base32() {
        for _ in 0..200 {
            let code = new_code().unwrap();
            assert_eq!(code.len(), 8);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
            assert_eq!(normalize(&code).as_deref(), Some(code.as_str()));
        }
        assert_eq!(normalize("k7qm-4ztd").as_deref(), Some("K7QM4ZTD"));
        assert_eq!(normalize(" K7QM 4ZTD ").as_deref(), Some("K7QM4ZTD"));
        assert_eq!(normalize("O1IL-oilx").as_deref(), Some("0111011X"));
        for bad in [
            "",
            "K7QM-4ZT",
            "K7QM-4ZTDX",
            "K7QM-4ZTU",
            "K7QM_4ZTD",
            "K7QM-4ZTÉ",
        ] {
            assert_eq!(normalize(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_code_works_once() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        let (id, code) = waiting(&mut p, t0);
        assert_eq!(
            p.status(&id, t0),
            Some(Status::Waiting {
                expires_in: LIFETIME.as_secs()
            })
        );
        let claim = p.redeem(&code, t0).unwrap();
        assert_eq!(
            (claim.id.as_str(), claim.name.as_str()),
            (id.as_str(), "Phone")
        );
        assert_eq!(p.redeem(&code, t0), Err(Refused::Used));
        p.paired(&id, 7);
        assert_eq!(p.status(&id, t0), Some(Status::Paired { device: 7 }));
        assert_eq!(p.redeem(&code, t0), Err(Refused::Used));
        // Closing the dialog forgets it.
        p.close(&id);
        assert_eq!(p.status(&id, t0), None);
        assert_eq!(p.redeem(&code, t0), Err(Refused::Wrong));
    }

    #[test]
    fn codes_expire_after_ten_minutes() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        let (id, code) = waiting(&mut p, t0);
        let almost = t0 + LIFETIME - Duration::from_secs(1);
        assert_eq!(
            p.status(&id, almost),
            Some(Status::Waiting { expires_in: 1 })
        );
        let late = t0 + LIFETIME;
        assert_eq!(p.status(&id, late), Some(Status::Expired));
        assert_eq!(p.redeem(&code, late), Err(Refused::Expired));
        // Forgotten a while later.
        let later = t0 + LIFETIME + LINGER;
        assert_eq!(p.status(&id, later), None);
        assert_eq!(p.redeem(&code, later), Err(Refused::Wrong));
    }

    #[test]
    fn ten_failures_cancel_every_waiting_code() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        let (a, code_a) = waiting(&mut p, t0);
        let (b, _) = waiting(&mut p, t0);
        for _ in 0..FAILURES - 1 {
            assert_eq!(p.redeem("00000000", t0), Err(Refused::Wrong));
        }
        assert!(matches!(p.status(&a, t0), Some(Status::Waiting { .. })));
        assert_eq!(p.redeem("00000000", t0), Err(Refused::Wrong));
        for id in [&a, &b] {
            assert_eq!(
                p.status(id, t0),
                Some(Status::Cancelled {
                    why: Cancelled::Failures
                })
            );
        }
        assert_eq!(p.redeem(&code_a, t0), Err(Refused::Expired));
        // A new code starts the count afresh.
        let (c, code_c) = waiting(&mut p, t0);
        for _ in 0..FAILURES - 1 {
            let _ = p.redeem("00000000", t0);
        }
        assert!(p.redeem(&code_c, t0).is_ok());
        assert_eq!(p.status(&c, t0), Some(Status::Pairing));
    }

    #[test]
    fn trying_a_used_code_again_is_no_guess() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        let (id, used) = waiting(&mut p, t0);
        p.redeem(&used, t0).unwrap();
        p.paired(&id, 1);
        let (other, _) = waiting(&mut p, t0);
        // A device whose answer was lost tries its code again, and again.
        for _ in 0..FAILURES * 2 {
            assert_eq!(p.redeem(&used, t0), Err(Refused::Used));
        }
        assert!(matches!(p.status(&other, t0), Some(Status::Waiting { .. })));
    }

    #[test]
    fn failures_before_a_code_dont_count_against_it() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        for _ in 0..FAILURES - 1 {
            let _ = p.redeem("00000000", t0);
        }
        let (id, _) = waiting(&mut p, t0);
        let _ = p.redeem("00000000", t0);
        assert!(matches!(p.status(&id, t0), Some(Status::Waiting { .. })));
    }

    #[test]
    fn cancelling_and_limits() {
        let t0 = Instant::now();
        let mut p = Pairings::default();
        let (id, code) = waiting(&mut p, t0);
        let (other, _) = waiting(&mut p, t0);
        let mut sent = vec!["00000000".to_owned(), code.clone(), "ZZZZZZZZ".to_owned()];
        sent.sort();
        p.cancel_codes(&sent, Cancelled::Insecure, t0);
        assert_eq!(
            p.status(&id, t0),
            Some(Status::Cancelled {
                why: Cancelled::Insecure
            })
        );
        assert_eq!(p.redeem(&code, t0), Err(Refused::Expired));
        // Only the ones sent.
        assert!(matches!(p.status(&other, t0), Some(Status::Waiting { .. })));
        // Replacing a device that's removed meanwhile: the code is cancelled,
        // and the dialog says why.
        let (r, code) = p
            .create("Laptop".into(), Scope::Manage, Some(3), t0)
            .unwrap()
            .unwrap();
        p.forget_device(3);
        assert_eq!(
            p.status(&r, t0),
            Some(Status::Cancelled {
                why: Cancelled::Removed
            })
        );
        assert_eq!(p.redeem(&code, t0), Err(Refused::Expired));
        // Or, once claimed, when its token can't be issued for that reason.
        let (r, code) = p
            .create("Laptop".into(), Scope::Manage, Some(4), t0)
            .unwrap()
            .unwrap();
        assert!(p.redeem(&code, t0).is_ok());
        p.replaced_gone(&r);
        assert_eq!(
            p.status(&r, t0),
            Some(Status::Cancelled {
                why: Cancelled::Removed
            })
        );
        let mut full = Pairings::default();
        for _ in 0..MAX_WAITING {
            waiting(&mut full, t0);
        }
        assert!(
            full.create("One more".into(), Scope::Sync, None, t0)
                .unwrap()
                .is_none()
        );
        // Expired ones don't count.
        assert!(
            full.create("One more".into(), Scope::Sync, None, t0 + LIFETIME)
                .unwrap()
                .is_some()
        );
    }
}
