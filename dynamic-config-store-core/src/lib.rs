//! Machinery the [`dynamic-config`] store crates share.
//!
//! **This crate has no stable API.** It is published only because the seven
//! store crates are published and cargo will not let a published crate depend
//! on one that is not. Depend on [`dynamic-config-consul`], [`-etcd`],
//! [`-firestore`], [`-nats`], [`-redis`], [`-s3`] or [`-vault`]; nothing here
//! is meant to be named directly, and anything here may change in a patch
//! release.
//!
//! What lives here is what was *identical* in more than one store crate and
//! carried a decision worth making once:
//!
//! - [`attempts`] — where a watch loop reports an attempt that came back
//!   with nothing, so a store that has stopped answering stops looking
//!   healthy. Seven loops, one line each.
//! - [`credential`] — when to obtain, reuse and refresh a token that expires.
//!   Consul, Vault and Firestore each kept a copy; the differences between
//!   them stayed in the stores, and that module's documentation says which
//!   and why.
//! - [`documents`] — folding several keys into the one document `fetch`
//!   returns: the ordering rule, the collision report, and the limits an
//!   untrusted key list is held to. Three stores read several keys and all
//!   three would otherwise have written the same merge.
//! - [`guarded`] — running a watch callback with a panic net. Seven copies,
//!   byte for byte.
//! - [`redacted`] and [`redacted_list`] — removing a credential from a store
//!   URL before it reaches an error message. Two copies, differing in one
//!   documented way that is now a parameter rather than a fork.
//! - [`tls`] — a custom certificate authority and a client certificate, as
//!   data rather than as whichever type the store's client happens to use.
//!   Not a deduplication of seven copies: there were none, and four stores
//!   had no way to say it at all.
//!
//! What is deliberately *not* here: each store's retry policy, its timeout
//! default, and the vocabulary it sorts its own failures with. Those look
//! alike from a distance and are different decisions up close — a Vault 403
//! and a Firestore 401 mean the same thing to a person and nothing to a
//! `match`.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-consul`]: https://docs.rs/dynamic-config-consul
//! [`-etcd`]: https://docs.rs/dynamic-config-etcd
//! [`-firestore`]: https://docs.rs/dynamic-config-firestore
//! [`-nats`]: https://docs.rs/dynamic-config-nats
//! [`-redis`]: https://docs.rs/dynamic-config-redis
//! [`-s3`]: https://docs.rs/dynamic-config-s3
//! [`-vault`]: https://docs.rs/dynamic-config-vault

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod attempts;
pub mod credential;
pub mod documents;
pub mod tls;

use dynamic_config::{Error, Fetched};

/// Runs the watch callback with a panic net.
///
/// The callback is the caller's code on the caller's thread; a panic in it
/// used to unwind through the watch loop and kill that thread with the
/// `RemoteWatch` handle still looking alive. Caught, it becomes an orderly
/// error: the watch ends, and the caller is told why.
///
/// # Errors
///
/// Whatever the callback returns, or a `Remote` error naming the store if it
/// panicked.
pub fn guarded<F>(on_change: &mut F, document: Fetched, described: &str) -> Result<(), Error>
where
    F: FnMut(Fetched) -> Result<(), Error>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_change(document))).unwrap_or_else(
        |_| {
            Err(Error::remote(format!(
                "{described}: the watch callback panicked; the watch is stopped"
            )))
        },
    )
}

/// What a URL authority with no colon in it means.
///
/// `scheme://something@host` is a shape more than one store accepts, and the
/// two stores that accept it disagree about what `something` is. Naming the
/// disagreement is the point: it is one line of difference between NATS and
/// Redis, and it used to be two copies of the whole function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoneAuthority {
    /// The whole thing is the credential — NATS' `nats://token@host`.
    Secret,
    /// The whole thing is a user name, and the password is elsewhere or
    /// absent — Redis' `redis://user@host`.
    Username,
}

/// A URL with its credentials removed, for error messages.
///
/// `redis://user:hunter2@host` in a log is a credential in a log, and a store
/// URL is quoted into every error message and into `Debug`.
///
/// A URL this cannot parse is returned unchanged rather than blanked: the
/// shapes below are the ones that carry a credential, and something that is
/// not one of them is a string the caller needs to see to fix their
/// configuration.
#[must_use]
pub fn redacted(url: &str, lone: LoneAuthority) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };

    // `rsplit_once`, not `split_once`: a password may itself contain `@`
    // (`redis://user:p@ss@host`), and splitting on the *first* one would keep
    // the tail of the password in the "redacted" output.
    let Some((authority, tail)) = rest.rsplit_once('@') else {
        return url.to_owned();
    };

    match authority.split_once(':') {
        // A user:password pair keeps the user, which is the half worth seeing.
        Some((user, _)) => format!("{scheme}://{user}:***@{tail}"),
        None => match lone {
            LoneAuthority::Secret => format!("{scheme}://***@{tail}"),
            LoneAuthority::Username => format!("{scheme}://{authority}:***@{tail}"),
        },
    }
}

/// A comma-separated list of URLs, each redacted by [`redacted`].
///
/// NATS accepts a list of servers in one string, so redacting the string as a
/// whole would leave every server but the last one intact.
#[must_use]
pub fn redacted_list(urls: &str, lone: LoneAuthority) -> String {
    urls.split(',')
        .map(|url| redacted(url, lone))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use dynamic_config::Format;

    use super::*;

    #[test]
    fn a_panicking_callback_ends_the_watch_rather_than_the_thread() {
        let mut on_change = |_: Fetched| -> Result<(), Error> { panic!("the caller's bug") };

        let error = guarded(
            &mut on_change,
            Fetched::new("{}".to_owned(), Format::Json),
            "store the-key",
        )
        .expect_err("a panic becomes an error");

        assert!(error.to_string().contains("store the-key"), "{error}");
        assert!(error.to_string().contains("panicked"), "{error}");
    }

    #[test]
    fn a_callbacks_own_error_is_passed_through_unchanged() {
        let mut on_change =
            |_: Fetched| -> Result<(), Error> { Err(Error::remote("bad document")) };

        let error = guarded(
            &mut on_change,
            Fetched::new("{}".to_owned(), Format::Json),
            "store the-key",
        )
        .expect_err("the callback failed");

        assert_eq!(error.to_string(), "bad document");
    }

    #[test]
    fn a_password_never_reaches_an_error_message() {
        for lone in [LoneAuthority::Secret, LoneAuthority::Username] {
            assert_eq!(
                redacted("redis://app:hunter2@redis.internal:6379", lone),
                "redis://app:***@redis.internal:6379"
            );
            // A password may contain `@`; splitting on the first one would
            // leave its tail in the "redacted" output.
            assert_eq!(
                redacted("redis://app:p@ss@w@rd@redis.internal:6379", lone),
                "redis://app:***@redis.internal:6379"
            );
            assert_eq!(
                redacted("redis://redis.internal:6379", lone),
                "redis://redis.internal:6379"
            );
            assert_eq!(redacted("not a url", lone), "not a url");
        }
    }

    #[test]
    fn a_lone_authority_is_read_the_way_its_store_reads_it() {
        // NATS' `nats://token@host`: the whole authority is the secret.
        assert_eq!(
            redacted(
                "nats://hunter2-token@nats.internal:4222",
                LoneAuthority::Secret
            ),
            "nats://***@nats.internal:4222"
        );
        // Redis' `redis://user@host`: the whole authority is a user name, and
        // blanking it would hide the half worth seeing.
        assert_eq!(
            redacted("redis://app@redis.internal:6379", LoneAuthority::Username),
            "redis://app:***@redis.internal:6379"
        );
    }

    #[test]
    fn every_server_in_a_list_is_redacted() {
        assert_eq!(
            redacted_list(
                "nats://hunter2@a:4222,nats://hunter2@b:4222",
                LoneAuthority::Secret
            ),
            "nats://***@a:4222,nats://***@b:4222"
        );
    }
}
