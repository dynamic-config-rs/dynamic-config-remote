//! Authentication, against a scripted git host.
//!
//! ```text
//! cargo test -p dynamic-config-git --test over_http
//! ```
//!
//! No Docker and no network: the host is [`common::host`], a `TcpListener`
//! delegating the git half to a real `git upload-pack`. So the *protocol* is
//! genuine — the same one GitHub serves — and only the parts a test needs to
//! control are ours: which token is currently accepted, and what was asked
//! for.
//!
//! The `file://` transport in `against_a_repository.rs` cannot cover any of
//! this, because it has no authentication at all. TLS is `over_https.rs`,
//! against the same host behind a `rustls` listener.
//!
//! # Why plain HTTP is safe here
//!
//! `gix` refuses to send credentials over `http://` unless
//! `gix-transport/http-client-insecure-credentials` is on, and this crate does
//! not turn it on — a published `dynamic-config-git` will not put a token on
//! the wire in clear text. The feature is enabled through a **dev-dependency**,
//! so it exists for these tests, whose wire is a loopback socket, and for
//! nothing else.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::host::{self, Host, Serving};
use common::{Repository, Sandbox};
use dynamic_config::{ErrorKind, RemoteSource};
use dynamic_config_git::{Auth, Credential, GitSource};
use dynamic_config_store_core::credential::{Issued, REFRESH_WITHIN};

/// A repository with one configuration file, served over HTTP.
fn a_served_repository(
    named: &str,
    token: &str,
) -> (Sandbox, Repository, Arc<Host>, String, Serving) {
    let sandbox = Sandbox::new(named);
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit("config.yaml", "app:\n  host: db.internal\n");

    let host = Host::new(token);
    let (url, serving) = host::serve(&repository, Arc::clone(&host), "http", host::plain());

    (sandbox, repository, host, url, serving)
}

#[test]
fn a_token_gets_the_file_a_public_client_cannot_have() {
    let (sandbox, _repository, _host, url, _serving) = a_served_repository("token", "ghs_first");

    let source = GitSource::builder(&url)
        .path("config.yaml")
        .credential(Credential::token("ghs_first"))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(source.fetch().unwrap().text.contains("db.internal"));
}

/// **The test this crate exists for.**
///
/// A GitHub App installation token lives one hour and a watcher lives for the
/// life of the process, so the credential has to be able to change under a
/// running source. Here the closure hands out `ghs_first` and then
/// `ghs_second`, and the host stops accepting the first one between the two
/// fetches. The second fetch must succeed — reactively, because the token had
/// not reached its stated expiry and nothing but the host's refusal says it is
/// dead.
#[test]
fn a_token_the_host_stops_accepting_is_replaced_and_the_fetch_succeeds() {
    let (sandbox, _repository, host, url, _serving) = a_served_repository("rotated", "ghs_first");

    let issued = AtomicUsize::new(0);
    let credential = Credential::expiring(move |previous| {
        assert!(
            previous.is_none(),
            "a credential the host refused must not be offered back for renewal"
        );

        let count = issued.fetch_add(1, Ordering::SeqCst);

        Ok(Issued {
            value: Auth::Https {
                username: "x-access-token".to_owned(),
                // An hour, so nothing but the refusal can prompt a second one.
                password: ["ghs_first", "ghs_second"][count.min(1)].to_owned(),
            },
            ttl: Some(Duration::from_secs(3600)),
        })
    });

    let source = GitSource::builder(&url)
        .path("config.yaml")
        .credential(credential)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(source.fetch().unwrap().text.contains("db.internal"));

    host.now_accepts("ghs_second");

    assert!(
        source.fetch().unwrap().text.contains("db.internal"),
        "a token the host revoked must cost one refusal, not an outage"
    );

    let presented = host.presented();

    assert!(
        presented.contains(&"ghs_first".to_owned()) && presented.contains(&"ghs_second".to_owned()),
        "both tokens should have been on the wire: {presented:?}"
    );
}

/// The proactive half: a token that says how long it lives is replaced before
/// it expires, without anybody being refused.
#[test]
fn a_token_that_is_about_to_expire_is_replaced_before_it_is_used() {
    let (sandbox, _repository, host, url, _serving) = a_served_repository("expiring", "ghs_first");

    let issued = AtomicUsize::new(0);
    let credential = Credential::expiring(move |_previous| {
        let count = issued.fetch_add(1, Ordering::SeqCst);

        Ok(Issued {
            value: Auth::Https {
                username: "x-access-token".to_owned(),
                password: ["ghs_first", "ghs_second"][count.min(1)].to_owned(),
            },
            // Inside the refresh margin from the moment it is issued, which is
            // what a token with a short life looks like.
            ttl: Some(REFRESH_WITHIN / 2),
        })
    });

    let source = GitSource::builder(&url)
        .path("config.yaml")
        .credential(credential)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    source.fetch().unwrap();

    // The host now accepts only the second token, and has refused nothing:
    // the source must have obtained it on its own.
    host.now_accepts("ghs_second");
    source
        .fetch()
        .expect("the refresh happens before the request");

    assert_eq!(
        host.refusals(),
        0,
        "the second token was obtained before it was needed, so nothing was \
         ever refused: {:?}",
        host.presented()
    );
    assert!(
        host.presented().contains(&"ghs_second".to_owned()),
        "the replacement must actually have been used: {:?}",
        host.presented()
    );
}

/// The idle case, measured rather than claimed: a ref that has not moved costs
/// a ref advertisement and no object transfer at all. This is what makes
/// polling a git host at a one-minute cadence defensible.
#[test]
fn an_unchanged_ref_transfers_no_objects() {
    let (sandbox, _repository, host, url, _serving) = a_served_repository("unchanged", "ghs_token");

    let source = GitSource::builder(&url)
        .path("config.yaml")
        .credential(Credential::token("ghs_token"))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    source.fetch().unwrap();
    assert_eq!(host.transfers(), 1, "the first fetch has to transfer");

    source.fetch().unwrap();
    source.fetch().unwrap();

    assert_eq!(
        host.transfers(),
        1,
        "an unchanged ref must cost a ref advertisement and nothing else"
    );
}

/// A wrong token is `Auth`, not `Remote` — the distinction a reload loop uses
/// to stop rather than back off — and it does not appear in the message.
#[test]
fn a_refused_token_is_an_auth_error_that_does_not_quote_the_token() {
    let (sandbox, _repository, _host, url, _serving) = a_served_repository("refused", "ghs_right");

    let error = GitSource::builder(&url)
        .path("config.yaml")
        .credential(Credential::token("ghs_hunter2-wrong"))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("the host does not accept that token");

    assert_eq!(
        error.kind(),
        ErrorKind::Auth,
        "waiting does not fix a wrong token: {error}"
    );
    assert!(!error.to_string().contains("hunter2"), "{error}");
}

/// A watch loop must not hammer a host with a token that cannot change. When
/// the credential is a constant the host refuses, the loop ends and says why.
#[test]
fn a_watch_ends_rather_than_retrying_a_credential_nothing_can_replace() {
    let (sandbox, _repository, _host, url, _serving) = a_served_repository("hammer", "ghs_right");

    let source = GitSource::builder(&url)
        .path("config.yaml")
        .credential(Credential::token("ghs_hunter2-wrong"))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let watch = dynamic_config::RemoteWatch::new();
    let error = source
        .watch(&watch.watching(), Duration::from_millis(50), |_| Ok(()))
        .expect_err("a constant token the host refuses cannot come right");

    assert_eq!(error.kind(), ErrorKind::Auth, "{error}");
    assert!(!error.to_string().contains("hunter2"), "{error}");
}
