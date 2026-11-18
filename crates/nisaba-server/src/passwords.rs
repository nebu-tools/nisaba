//! Password work and the limits around it.
//!
//! - A hash or check runs on a blocking thread, at most `workers` at once.
//!   Each takes about 19 MiB and a core.
//! - At most `queue` more requests wait for a worker. One more is refused at
//!   once with [`Busy`], which the routes answer with 503 and Retry-After.
//! - Each email that fails to sign in gets a growing delay: the first
//!   [`FREE_FAILURES`] failures cost nothing, then the next attempt must wait
//!   1 s, 2 s, 4 s and so on up to [`MAX_DELAY`]. A sign-in that works clears
//!   it, and an email quiet for [`FORGET_AFTER`] starts over. At most
//!   [`TRACKED`] emails are kept; past that the oldest is dropped.
//!
//! This slows guessing one account's password. It does not stop a flood of
//! sign-ins spread over many emails; SECURITY.md says what does.
//!
//! Hashes are Argon2id with the parameters [`Passwords::new`] is given; the
//! defaults are the `argon2` crate's (19 MiB, 2 passes, 1 lane). A hash made
//! with other parameters still verifies, and [`Passwords::outdated`] says
//! when sign-in should store a new one.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::Semaphore;

/// Failures of one email before the delay starts.
pub const FREE_FAILURES: u32 = 3;
/// The longest delay between two sign-ins of one email.
pub const MAX_DELAY: Duration = Duration::from_secs(300);
/// An email with no failure for this long starts over.
pub const FORGET_AFTER: Duration = Duration::from_secs(3600);
/// Emails whose failures are kept at once.
pub const TRACKED: usize = 10_000;

/// Every worker is busy and the queue is full.
#[derive(Debug)]
pub struct Busy;

pub struct Passwords {
    workers: Semaphore,
    places: Semaphore,
    failures: Mutex<HashMap<String, Failures>>,
    computed: AtomicU64,
    params: Params,
    /// Checked when an email has no account, so a wrong email takes as long
    /// as a wrong password.
    dummy: String,
}

/// The Argon2id parameters: memory in KiB and passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub memory_kib: u32,
    pub iterations: u32,
}

impl Default for Cost {
    /// The `argon2` crate's defaults.
    fn default() -> Self {
        Self {
            memory_kib: Params::DEFAULT_M_COST,
            iterations: Params::DEFAULT_T_COST,
        }
    }
}

impl Cost {
    /// The parameters, refused below the defaults.
    pub fn params(self) -> Result<Params, String> {
        let low = Self::default();
        if self.memory_kib < low.memory_kib || self.iterations < low.iterations {
            return Err(format!(
                "password hashing needs at least {} KiB and {} passes",
                low.memory_kib, low.iterations
            ));
        }
        Params::new(self.memory_kib, self.iterations, 1, None).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy)]
struct Failures {
    count: u32,
    last: Instant,
}

impl Failures {
    /// When the next attempt may run.
    fn open_at(self) -> Instant {
        self.last + delay(self.count)
    }
}

/// The wait after `count` failures in a row.
fn delay(count: u32) -> Duration {
    if count < FREE_FAILURES {
        return Duration::ZERO;
    }
    let doublings = (count - FREE_FAILURES).min(16);
    Duration::from_secs(1 << doublings).min(MAX_DELAY)
}

impl Passwords {
    /// Hashes with `cost`, which [`Cost::params`] checks.
    pub fn new(workers: usize, queue: usize, cost: Cost) -> Result<Self, String> {
        let params = cost.params()?;
        let dummy = hash_with(&params, "no account has this password")?;
        Ok(Self {
            workers: Semaphore::new(workers),
            places: Semaphore::new(workers + queue),
            failures: Mutex::default(),
            computed: AtomicU64::new(0),
            params,
            dummy,
        })
    }

    /// An Argon2id hash of `password` in PHC string form, on a worker.
    pub async fn hash(&self, password: String) -> Result<Result<String, String>, Busy> {
        let params = self.params.clone();
        self.run(move || hash_with(&params, &password)).await
    }

    /// Whether `password` matches `hash`, on a worker. `None` checks against
    /// a fixed hash and is never true, so an unknown email costs the same.
    pub async fn verify(&self, password: String, hash: Option<String>) -> Result<bool, Busy> {
        let known = hash.is_some();
        let hash = hash.unwrap_or_else(|| self.dummy.clone());
        let matches = self.run(move || verify(&password, &hash)).await?;
        Ok(known && matches)
    }

    /// Whether `hash` was made with other parameters than the current ones,
    /// so that a sign-in should store a new hash of the password.
    #[must_use]
    pub fn outdated(&self, hash: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(hash) else {
            return true;
        };
        parsed.algorithm.as_str() != "argon2id"
            || Params::try_from(&parsed).map_or(true, |p| {
                p.m_cost() != self.params.m_cost() || p.t_cost() != self.params.t_cost()
            })
    }

    /// Runs `work` on a blocking thread when a worker is free, or refuses at
    /// once when the queue is full.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Busy> {
        let _place = self.places.try_acquire().map_err(|_| Busy)?;
        let _worker = self.workers.acquire().await.map_err(|_| Busy)?;
        self.computed.fetch_add(1, Ordering::Relaxed);
        tokio::task::spawn_blocking(work).await.map_err(|_| Busy)
    }

    /// How many hashes or checks have started. For tests.
    #[must_use]
    pub fn computed(&self) -> u64 {
        self.computed.load(Ordering::Relaxed)
    }

    /// `Err(wait)` when `email` must wait before it may try again.
    pub fn check(&self, email: &str, now: Instant) -> Result<(), Duration> {
        let failures = self.lock();
        match failures.get(email) {
            Some(f) if f.open_at() > now => Err(f.open_at() - now),
            _ => Ok(()),
        }
    }

    /// Records a failed sign-in of `email`.
    pub fn failed(&self, email: &str, now: Instant) {
        let mut failures = self.lock();
        if !failures.contains_key(email) && failures.len() >= TRACKED {
            failures.retain(|_, f| now.saturating_duration_since(f.last) < FORGET_AFTER);
            if failures.len() >= TRACKED
                && let Some(oldest) = failures
                    .iter()
                    .min_by_key(|(_, f)| f.last)
                    .map(|(k, _)| k.clone())
            {
                failures.remove(&oldest);
            }
        }
        let entry = failures.entry(email.to_owned()).or_insert(Failures {
            count: 0,
            last: now,
        });
        if now.saturating_duration_since(entry.last) >= FORGET_AFTER {
            entry.count = 0;
        }
        entry.count += 1;
        entry.last = now;
    }

    /// Clears the failures of `email` after a sign-in that worked.
    pub fn succeeded(&self, email: &str) {
        self.lock().remove(email);
    }

    /// How many emails have failures kept.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Failures>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An Argon2id hash of `password` with `params`.
pub fn hash_with(params: &Params, password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone())
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Whether `password` matches `hash`, with the parameters the hash names.
#[must_use]
pub fn verify(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passwords(workers: usize, queue: usize) -> Passwords {
        Passwords::new(workers, queue, Cost::default()).unwrap()
    }

    /// A hash made today verifies after the parameters are raised, and is
    /// then outdated.
    #[tokio::test]
    async fn a_hash_with_old_parameters_verifies_and_is_outdated() {
        let today = passwords(1, 0);
        let hash = today.hash("periwinkle".into()).await.unwrap().unwrap();
        assert!(!today.outdated(&hash));
        let raised = Passwords::new(
            1,
            0,
            Cost {
                memory_kib: Cost::default().memory_kib * 2,
                iterations: 3,
            },
        )
        .unwrap();
        assert!(
            raised
                .verify("periwinkle".into(), Some(hash.clone()))
                .await
                .unwrap()
        );
        assert!(
            !raised
                .verify("winkle".into(), Some(hash.clone()))
                .await
                .unwrap()
        );
        assert!(raised.outdated(&hash));
        let new = raised.hash("periwinkle".into()).await.unwrap().unwrap();
        assert!(!raised.outdated(&new));
        assert!(new.contains("m=38912,t=3"), "{new}");
        assert!(!raised.verify("periwinkle".into(), None).await.unwrap());
        assert!(
            Passwords::new(
                1,
                0,
                Cost {
                    memory_kib: 1024,
                    iterations: 2
                }
            )
            .is_err(),
            "weaker than the defaults"
        );
    }

    #[test]
    fn the_delay_grows_after_the_free_failures() {
        let p = passwords(1, 0);
        let t = Instant::now();
        for _ in 0..FREE_FAILURES - 1 {
            p.failed("a@x", t);
            assert!(p.check("a@x", t).is_ok());
        }
        p.failed("a@x", t);
        assert_eq!(p.check("a@x", t), Err(Duration::from_secs(1)));
        assert!(p.check("a@x", t + Duration::from_secs(1)).is_ok());
        let t = t + Duration::from_secs(1);
        p.failed("a@x", t);
        assert_eq!(p.check("a@x", t), Err(Duration::from_secs(2)));
        p.failed("a@x", t);
        assert_eq!(p.check("a@x", t), Err(Duration::from_secs(4)));
        for _ in 0..40 {
            p.failed("a@x", t);
        }
        assert_eq!(p.check("a@x", t), Err(MAX_DELAY));
        assert!(p.check("b@x", t).is_ok(), "another email is not slowed");
    }

    #[test]
    fn a_sign_in_that_works_or_a_quiet_hour_starts_over() {
        let p = passwords(1, 0);
        let t = Instant::now();
        for _ in 0..FREE_FAILURES + 2 {
            p.failed("a@x", t);
        }
        assert!(p.check("a@x", t).is_err());
        p.succeeded("a@x");
        assert!(p.check("a@x", t).is_ok());
        for _ in 0..FREE_FAILURES + 2 {
            p.failed("b@x", t);
        }
        let later = t + FORGET_AFTER;
        p.failed("b@x", later);
        assert!(
            p.check("b@x", later).is_ok(),
            "one failure after a quiet hour"
        );
    }

    #[test]
    fn memory_is_bounded() {
        let p = passwords(1, 0);
        let t = Instant::now();
        for i in 0..TRACKED + 500 {
            p.failed(&format!("{i}@x"), t + Duration::from_millis(i as u64));
        }
        assert_eq!(p.tracked(), TRACKED);
        assert!(p.lock().contains_key(&format!("{}@x", TRACKED + 499)));
        assert!(!p.lock().contains_key("0@x"), "the oldest went first");
    }

    #[tokio::test]
    async fn a_full_queue_is_refused_at_once() {
        let p = std::sync::Arc::new(passwords(1, 1));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let busy = {
            let p = p.clone();
            tokio::spawn(async move { p.run(move || wait.recv().ok()).await })
        };
        while p.computed() == 0 {
            tokio::task::yield_now().await;
        }
        let queued = {
            let p = p.clone();
            tokio::spawn(async move { p.run(|| 2).await })
        };
        tokio::task::yield_now().await;
        while p.places.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            p.run(|| 3).await.is_err(),
            "the third request waits for nothing"
        );
        release.send(()).unwrap();
        assert!(busy.await.unwrap().is_ok());
        assert_eq!(queued.await.unwrap().unwrap(), 2);
        assert_eq!(p.run(|| 4).await.unwrap(), 4);
    }
}
