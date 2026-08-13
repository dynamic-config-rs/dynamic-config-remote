//! A credential that expires, and getting another one before it does.
//!
//! Three store crates cache a token: Consul, Vault and Firestore. They used to
//! keep three copies of the same `Session`/`Token` pair, and the copies had
//! drifted only in the ways their stores actually differ. This is the part
//! that was the same in all three — *when* to obtain a credential — with the
//! parts that were not left where they belong, in the store.
//!
//! # What the three stores agree on
//!
//! | | Consul | Vault | Firestore |
//! |---|---|---|---|
//! | Refreshed within [`REFRESH_WITHIN`] of expiry | ✓ | ✓ | ✓ |
//! | Expiry is a local `Instant` plus a server-reported TTL | ✓ | ✓ | ✓ |
//! | No TTL means never refreshed | ✓ | ✓ | ✓ |
//! | A TTL too large to represent means no expiry | ✓ | ✓ | ✓ |
//! | One `Mutex`, held across obtaining | ✓ | ✓ | ✓ |
//! | A failed obtain leaves the old credential in place | ✓ | ✓ | ✓ |
//! | Reactively invalidated after the store refuses it | ✓ | ✓ | ✓ |
//!
//! That is [`Cached<T>`]: seven rows, no exceptions.
//!
//! # What they do not agree on, and where it stays
//!
//! | | Consul | Vault | Firestore |
//! |---|---|---|---|
//! | What a refusal looks like | 403 | 403 | 401 |
//! | Can renew rather than re-obtain | — | when the token says so | — |
//! | A credential-free mode | `Auth::Anonymous` | — | `Auth::Emulator` |
//! | A credential handed in from outside | `Auth::Token` | `Auth::Token` | `Auth::AccessToken` |
//!
//! None of those four reach this module, and that is the design rather than an
//! omission:
//!
//! - **The refusal status** is read from the store's own typed HTTP error, which
//!   this crate never sees. Sorting a `ureq::Error` here would mean this crate
//!   knowing which status each service uses to mean *your token is dead*, which
//!   is exactly the knowledge that belongs beside the endpoint.
//! - **Renewal** is Vault's alone — Consul issues login tokens and expects
//!   another login, and Firestore's metadata server cannot extend anything. It
//!   is expressed by the `obtain` closure being handed the credential it is
//!   replacing: a store that can renew renews, and one that cannot ignores the
//!   argument.
//! - **The credential-free and handed-in modes never reach a cache at all.**
//!   `Auth::Anonymous` presents no token and `Auth::Token` presents the same
//!   string forever; wrapping either in a cache would only add a lock to a
//!   value that cannot change. Each store answers those before it asks here.
//!
//! # The margin is the only defence against clock skew
//!
//! Expiry is computed from a *local* `Instant` plus a *server-reported* TTL, so
//! any disagreement between the server's issue time and our receipt time comes
//! straight out of the margin. That is why it is a minute rather than a second.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dynamic_config::Error;

/// How close to expiry a credential may get before it is refreshed.
///
/// One name and one value across the token-caching store crates, on purpose.
/// The margin is also the only cushion against clock skew: expiry is computed
/// from a *local* `Instant` plus a *server-reported* TTL, so any disagreement
/// between the server's issue time and our receipt time eats into it. A minute
/// absorbs the skew a real fleet actually has.
pub const REFRESH_WITHIN: Duration = Duration::from_secs(60);

/// Where a Kubernetes service-account token is mounted, by convention.
pub const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// What an `obtain` returns: the credential, and how long the server says it
/// lives.
///
/// `ttl` is `None` for a credential with no expiry — a Vault root token, a
/// Consul token its auth method put no expiry on. Filtering a server's zero
/// into `None` is the store's job, because zero means *does not expire* in
/// Vault's vocabulary and *no answer* in nobody's.
pub struct Issued<T> {
    /// The credential itself.
    pub value: T,
    /// How long the server said it lives, from now.
    pub ttl: Option<Duration>,
}

/// What is currently held, and until when.
struct Held<T> {
    value: T,
    /// `None` for a credential that does not expire.
    expires_at: Option<Instant>,
}

/// A credential that expires and can be obtained again.
///
/// A `Mutex` rather than a lock-free cell: obtaining twice concurrently is
/// harmless but wasteful, and this sits on the once-per-refresh path rather
/// than the once-per-request one. The lock is held *across* obtaining, so N
/// threads arriving at an expired credential together produce one request
/// rather than N.
pub struct Cached<T> {
    held: Mutex<Option<Held<T>>>,
    margin: Duration,
}

impl<T> Cached<T> {
    /// An empty cache, refreshing within [`REFRESH_WITHIN`] of expiry.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_margin(REFRESH_WITHIN)
    }

    /// An empty cache with a margin of its own.
    ///
    /// For tests, which cannot wait a minute to watch a margin work.
    #[must_use]
    pub const fn with_margin(margin: Duration) -> Self {
        Self {
            held: Mutex::new(None),
            margin,
        }
    }

    /// Drops what is held, so the next [`get`](Self::get) obtains.
    ///
    /// The reactive door: a store that has just been told its credential is no
    /// longer accepted calls this and tries once more.
    pub fn invalidate(&self) {
        *self.lock() = None;
    }

    /// Whether this credential is close enough to expiry to replace.
    fn is_stale(&self, held: &Held<T>) -> bool {
        held.expires_at.is_some_and(|expires_at| {
            expires_at.saturating_duration_since(Instant::now()) < self.margin
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Held<T>>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: Clone> Cached<T> {
    /// The current credential, obtaining or refreshing it as needed.
    ///
    /// `obtain` is handed the credential it is replacing, if there is one and
    /// it has merely gone stale — which is what lets Vault renew a token
    /// rather than log in again. It is `None` on the first call and after
    /// [`invalidate`](Self::invalidate), because there is then nothing to
    /// extend.
    ///
    /// `obtain` is a closure rather than a trait method so this module stays
    /// free of HTTP: what it decides is *when*, not *how*.
    ///
    /// # Errors
    ///
    /// Whatever `obtain` reports. What was held survives a failed obtain: a
    /// credential that is merely close to expiry still works, and throwing it
    /// away because a refresh failed would turn a recoverable moment into an
    /// outage.
    pub fn get(
        &self,
        obtain: impl FnOnce(Option<&T>) -> Result<Issued<T>, Error>,
    ) -> Result<T, Error> {
        let mut held = self.lock();

        if let Some(current) = held.as_ref() {
            if !self.is_stale(current) {
                return Ok(current.value.clone());
            }
        }

        let issued = obtain(held.as_ref().map(|current| &current.value))?;
        let value = issued.value.clone();

        *held = Some(Held {
            value: issued.value,
            // `checked_add` because the TTL comes from the server: one
            // answering with a nonsense number would otherwise panic the
            // process on the arithmetic. Too large to represent is treated as
            // no expiry, which is what a number that large means anyway.
            expires_at: issued.ttl.and_then(|ttl| Instant::now().checked_add(ttl)),
        });

        Ok(value)
    }
}

impl<T> Default for Cached<T> {
    fn default() -> Self {
        Self::new()
    }
}

// Hand-written, never derived: `T` is a credential, and a derive would print
// it. `{:?}` reaching a log is an ordinary accident — a `dbg!`, a
// `tracing::debug!(?source)` — and an accident must not disclose a secret.
// `try_lock`, because a `Debug` that can block is a `Debug` that can deadlock
// the thread already holding the lock.
impl<T> std::fmt::Debug for Cached<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = match self.held.try_lock() {
            Ok(held) => held,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return f.debug_struct("Cached").finish_non_exhaustive();
            }
        };

        f.debug_struct("Cached")
            .field("value", &held.as_ref().map(|_| "***"))
            .field(
                "expires_at",
                &held.as_ref().and_then(|held| held.expires_at),
            )
            .field("margin", &self.margin)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// An `obtain` that counts its calls and hands back a credential with
    /// `ttl`.
    fn counted(
        calls: &AtomicUsize,
        ttl: Option<Duration>,
    ) -> impl Fn(Option<&String>) -> Result<Issued<String>, Error> + '_ {
        move |_| {
            let count = calls.fetch_add(1, Ordering::SeqCst);

            Ok(Issued {
                value: format!("token-{count}"),
                ttl,
            })
        }
    }

    #[test]
    fn a_credential_is_obtained_once_and_then_reused() {
        let calls = AtomicUsize::new(0);
        let cached = Cached::new();
        let obtain = counted(&calls, Some(Duration::from_secs(3600)));

        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_credential_inside_the_margin_is_obtained_again() {
        let calls = AtomicUsize::new(0);
        let cached = Cached::new();
        // Half the margin: still valid, and close enough that a request
        // starting now might outlive it.
        let obtain = counted(&calls, Some(REFRESH_WITHIN / 2));

        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(cached.get(&obtain).unwrap(), "token-1");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_credential_with_no_ttl_is_never_refreshed() {
        let calls = AtomicUsize::new(0);
        let cached = Cached::new();
        let obtain = counted(&calls, None);

        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a root token does not expire"
        );
    }

    #[test]
    fn a_ttl_too_large_to_represent_is_treated_as_no_expiry() {
        // A server answering with nonsense must not be able to panic the
        // process on `Instant + Duration`.
        let calls = AtomicUsize::new(0);
        let cached = Cached::new();
        let obtain = counted(&calls, Some(Duration::from_secs(u64::MAX)));

        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(cached.get(&obtain).unwrap(), "token-0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidating_forces_the_next_get_to_obtain() {
        let calls = AtomicUsize::new(0);
        let cached = Cached::new();
        let obtain = counted(&calls, Some(Duration::from_secs(3600)));

        assert_eq!(cached.get(&obtain).unwrap(), "token-0");

        cached.invalidate();

        assert_eq!(
            cached.get(&obtain).unwrap(),
            "token-1",
            "a refusal must be able to force a fresh credential"
        );
    }

    #[test]
    fn a_stale_credential_is_offered_to_obtain_so_it_can_be_renewed() {
        let cached = Cached::new();

        assert_eq!(
            cached
                .get(|previous| {
                    assert!(previous.is_none(), "there is nothing to renew yet");

                    Ok(Issued {
                        value: "first".to_owned(),
                        ttl: Some(REFRESH_WITHIN / 2),
                    })
                })
                .unwrap(),
            "first"
        );

        assert_eq!(
            cached
                .get(|previous| {
                    // Vault's renewal presents the token it is extending; it
                    // can only do that if the stale one is handed over.
                    assert_eq!(previous.map(String::as_str), Some("first"));

                    Ok(Issued {
                        value: "renewed".to_owned(),
                        ttl: Some(Duration::from_secs(3600)),
                    })
                })
                .unwrap(),
            "renewed"
        );
    }

    #[test]
    fn an_invalidated_credential_is_not_offered_to_obtain() {
        // The reactive path exists because the credential stopped working:
        // handing it back would invite a renewal of something the store has
        // already refused.
        let cached = Cached::new();

        cached
            .get(|_| {
                Ok(Issued {
                    value: "first".to_owned(),
                    ttl: Some(Duration::from_secs(3600)),
                })
            })
            .unwrap();

        cached.invalidate();

        cached
            .get(|previous| {
                assert!(previous.is_none(), "there is nothing left to renew");

                Ok(Issued {
                    value: "second".to_owned(),
                    ttl: None,
                })
            })
            .unwrap();
    }

    #[test]
    fn a_failed_obtain_leaves_the_previous_credential_in_place() {
        let cached = Cached::new();

        assert_eq!(
            cached
                .get(|_| Ok(Issued {
                    value: "first".to_owned(),
                    // Inside the margin, so the next `get` tries to replace it.
                    ttl: Some(REFRESH_WITHIN / 2),
                }))
                .unwrap(),
            "first"
        );

        let error = cached
            .get(|_| Err::<Issued<String>, _>(Error::remote("the store is away")))
            .expect_err("obtaining failed");

        assert!(error.to_string().contains("the store is away"), "{error}");

        cached
            .get(|previous| {
                assert_eq!(
                    previous.map(String::as_str),
                    Some("first"),
                    "a refresh that failed must not throw away a credential \
                     that still works"
                );

                Ok(Issued {
                    value: "second".to_owned(),
                    ttl: None,
                })
            })
            .unwrap();
    }

    /// The thundering herd: N threads arriving at an empty cache together
    /// must produce one login, not N. The lock is held across obtaining for
    /// exactly this reason, and none of the three stores tested it before the
    /// machinery lived in one place.
    #[test]
    fn concurrent_gets_obtain_once() {
        const THREADS: usize = 8;

        let calls = AtomicUsize::new(0);
        let cached: Cached<String> = Cached::new();

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let token = cached
                        .get(|_| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            // Long enough that every other thread is waiting
                            // on the lock by the time this returns.
                            std::thread::sleep(Duration::from_millis(50));

                            Ok(Issued {
                                value: "shared".to_owned(),
                                ttl: Some(Duration::from_secs(3600)),
                            })
                        })
                        .unwrap();

                    assert_eq!(token, "shared");
                });
            }
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "eight readers finding an empty cache is one login, not eight"
        );
    }

    #[test]
    fn debug_never_prints_the_credential() {
        let cached = Cached::new();

        cached
            .get(|_| {
                Ok(Issued {
                    value: "hunter2-token".to_owned(),
                    ttl: Some(Duration::from_secs(3600)),
                })
            })
            .unwrap();

        let printed = format!("{cached:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("***"), "{printed}");
    }

    #[test]
    fn debug_does_not_block_on_a_held_lock() {
        // A `Debug` that waits for the lock deadlocks the thread that already
        // holds it — a `dbg!` inside an `obtain` would hang the process.
        let cached: Cached<String> = Cached::new();

        cached
            .get(|_| {
                let printed = format!("{cached:?}");

                assert!(!printed.contains("hunter2"), "{printed}");

                Ok(Issued {
                    value: "hunter2-token".to_owned(),
                    ttl: None,
                })
            })
            .unwrap();
    }
}
