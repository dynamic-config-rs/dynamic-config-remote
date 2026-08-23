//! A ceiling on how many requests one source has out at once.
//!
//! # The number this exists for
//!
//! Five thousand pods, one Consul. Each agent runs a watch, a resync, an
//! auth refresh and — since 0.10 — a lease renewal, and every one of those
//! is a request the store has to answer. Individually they are nothing.
//! What makes them something is that they stop being independent: a store
//! that goes away and comes back is a store every one of those pods
//! reaches for in the same second.
//!
//! [`Pace`](dynamic_config::Pace) already spreads *that* — the waits are
//! jittered and grow after a failure, so the fleet does not arrive
//! together. This is the other half, and a smaller one: a bound on what a
//! *single* source can have in flight, so one agent having a bad minute
//! cannot open connections faster than it closes them.
//!
//! # Why it is a permit and not a queue
//!
//! A queue would make a caller wait its turn, which for a watch loop means
//! a delivery held behind a resync that is held behind an auth refresh.
//! Every one of those has its own retry, and none of them is improved by
//! being later. So a caller that cannot get a permit is told *no* and backs
//! off through the pace it already has — which is the mechanism this family
//! already uses for "the store is busy", and a second one would be a second
//! thing to reason about.
//!
//! Vault Agent's template engine has `max_connections_per_host` and
//! defaults it to ten. The default here is the same number, for the same
//! reason, arrived at by the same argument.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How many requests one source may have out at once, unless a caller says
/// otherwise.
pub const MAX_IN_FLIGHT: usize = 10;

/// How a store paces itself against one host.
#[derive(Debug)]
pub struct StoreRuntimePolicy {
    limit: usize,
    in_flight: AtomicUsize,
}

impl Default for StoreRuntimePolicy {
    fn default() -> Self {
        Self::new(MAX_IN_FLIGHT)
    }
}

impl StoreRuntimePolicy {
    /// A policy allowing `limit` requests at once.
    ///
    /// Zero means unbounded, which is what every version before this did
    /// and is occasionally what somebody wants — a single-tenant store
    /// nobody else is talking to.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Takes a permit, or `None` when the ceiling is reached.
    ///
    /// The permit puts itself back when it drops, including on a panic and
    /// on an early return — which is why it is a guard rather than a pair
    /// of calls somebody has to remember to balance.
    #[must_use]
    pub fn admit(&self) -> Option<InFlight<'_>> {
        if self.limit == 0 {
            return Some(InFlight { policy: None });
        }

        // A compare-and-swap loop rather than fetch_add-then-check: the
        // latter briefly counts a request that was refused, and a caller
        // reading `in_flight` for a metric would see a number that never
        // happened.
        let mut held = self.in_flight.load(Ordering::Acquire);

        loop {
            if held >= self.limit {
                return None;
            }

            match self.in_flight.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(InFlight { policy: Some(self) }),
                Err(actual) => held = actual,
            }
        }
    }

    /// How many are out right now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// The ceiling. Zero is unbounded.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// One request's permit, returned when it drops.
#[derive(Debug)]
pub struct InFlight<'a> {
    /// `None` for an unbounded policy, where there is nothing to give back.
    policy: Option<&'a StoreRuntimePolicy>,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(policy) = self.policy {
            policy.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_holds_and_the_permit_gives_itself_back() {
        let policy = StoreRuntimePolicy::new(2);

        let first = policy.admit().expect("under the ceiling");
        let second = policy.admit().expect("at the ceiling");

        assert_eq!(policy.in_flight(), 2);
        assert!(policy.admit().is_none(), "a third is past it");

        drop(second);

        assert_eq!(policy.in_flight(), 1);
        assert!(policy.admit().is_some(), "a permit came back");

        drop(first);
    }

    /// A refused request must not appear in the count. A caller reading it
    /// for a metric would otherwise see a number that never happened.
    #[test]
    fn a_refusal_does_not_move_the_count() {
        let policy = StoreRuntimePolicy::new(1);
        let held = policy.admit().expect("the first");

        assert!(policy.admit().is_none());
        assert_eq!(policy.in_flight(), 1);

        drop(held);
        assert_eq!(policy.in_flight(), 0);
    }

    /// Zero is unbounded, which is what every version before this did.
    #[test]
    fn zero_is_no_ceiling_at_all() {
        let policy = StoreRuntimePolicy::new(0);

        let permits: Vec<_> = (0..1000).map(|_| policy.admit()).collect();

        assert!(permits.iter().all(Option::is_some));
    }

    /// The permit comes back on a panic too, which is the whole reason it
    /// is a guard.
    #[test]
    fn a_panic_returns_the_permit() {
        let policy = StoreRuntimePolicy::new(1);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = policy.admit().expect("the first");

            panic!("something went wrong mid-request");
        }));

        assert!(outcome.is_err());
        assert_eq!(policy.in_flight(), 0, "the permit came back");
        assert!(policy.admit().is_some());
    }

    #[test]
    fn the_default_is_the_number_the_reference_implementation_uses() {
        assert_eq!(StoreRuntimePolicy::default().limit(), 10);
    }
}
