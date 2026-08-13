//! Against a real etcd, in a container.
//!
//! ```text
//! cargo test -p dynamic-config-etcd
//! ```
//!
//! Needs a working Docker daemon. `testcontainers-modules` has no etcd module,
//! so the image is described here — which is also the shape anyone reaching for
//! a store this crate does not ship a module for will need.

use dynamic_config::{AsyncRemoteSource, Format};
use dynamic_config_etcd::Etcd;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

/// quay.io rather than Docker Hub: no pull limits, no anonymous rate limiting.
const IMAGE: &str = "quay.io/coreos/etcd";
const TAG: &str = "v3.5.17";

struct Running {
    endpoint: String,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

/// `start()`, retried once with a fresh container.
///
/// On a busy shared runner the first boot occasionally loses the scheduling
/// lottery — `WaitContainer(StartupTimeout)` from a daemon that was going to
/// be fine in ten more seconds. One fresh attempt separates a slow neighbour
/// from an actual failure; failing twice is behaviour, and panics with both
/// errors.
async fn start_resilient<I, R>(make: impl Fn() -> R) -> testcontainers::ContainerAsync<I>
where
    I: testcontainers::Image,
    R: testcontainers::runners::AsyncRunner<I>,
{
    match make().start().await {
        Ok(container) => container,
        Err(first) => {
            eprintln!("container start failed ({first}); retrying once with a fresh container");
            // Not immediately: the retry that follows a lost scheduling
            // lottery without pausing is the attempt most likely to lose
            // the same one.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            make().start().await.unwrap_or_else(|second| {
                panic!(
                    "the container failed to start twice; is Docker available? \
                     first: {first}; then: {second}"
                )
            })
        }
    }
}

/// A container holding every `(key, value)` pair, written in one transaction.
async fn etcd_holding(pairs: &[(&str, &str)]) -> Running {
    let running = etcd_with(pairs[0].0, pairs[0].1).await;

    if pairs.len() > 1 {
        let mut client = etcd_client::Client::connect([running.endpoint.as_str()], None)
            .await
            .unwrap();

        client
            .txn(
                etcd_client::Txn::new().and_then(
                    pairs[1..]
                        .iter()
                        .map(|(key, value)| etcd_client::TxnOp::put(*key, *value, None))
                        .collect::<Vec<_>>(),
                ),
            )
            .await
            .expect("writing the rest of the keys should succeed");
    }

    running
}

async fn etcd_with(key: &str, value: &str) -> Running {
    let container = start_resilient(|| {
        GenericImage::new(IMAGE, TAG)
            .with_exposed_port(2379.tcp())
            .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
            .with_cmd([
                "etcd",
                "--advertise-client-urls=http://0.0.0.0:2379",
                "--listen-client-urls=http://0.0.0.0:2379",
            ])
    })
    .await;

    let port = container
        .get_host_port_ipv4(2379)
        .await
        .expect("etcd should expose its client port");
    let endpoint = format!("http://127.0.0.1:{port}");

    let mut client = etcd_client::Client::connect([endpoint.as_str()], None)
        .await
        .expect("the container should accept a connection");

    client
        .put(key, value, None)
        .await
        .expect("writing the key should succeed");

    Running {
        endpoint,
        _container: container,
    }
}

#[tokio::test]
async fn a_key_holds_a_whole_configuration_document() {
    let etcd = etcd_with(
        "myapp/db.json",
        r#"{"db": {"host": "localhost", "port": 5432}}"#,
    )
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/db.json")
        .await
        .expect("the container is up");

    let fetched = source.fetch().await.expect("the key is there");

    assert_eq!(
        fetched.format,
        Format::Json,
        "the format comes from the key's extension"
    );
    assert!(fetched.text.contains("localhost"), "{}", fetched.text);
}

#[tokio::test]
async fn the_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
    }

    let etcd = etcd_with(
        "myapp/loads.json",
        r#"{"db": {"host": "db.internal", "port": 6432}}"#,
    )
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/loads.json")
        .await
        .unwrap();
    let fetched = source.fetch().await.unwrap();

    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];
    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("the key holds a whole document");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
        }
    );
}

#[tokio::test]
async fn a_key_that_is_not_there_is_a_remote_error() {
    let etcd = etcd_with("myapp/present.json", r#"{"db": {}}"#).await;

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/absent.json")
        .await
        .unwrap();

    let error = source.fetch().await.expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("holds no value"), "{error}");
}

/// The client connects lazily, so an unreachable endpoint is not a construction
/// failure — it is a fetch failure. Asserted rather than worked around: pinning
/// the real behaviour beats documenting a wish.
#[tokio::test]
async fn an_unreachable_endpoint_fails_at_the_first_fetch() {
    // Port 1 is reserved and nothing serves gRPC on it.
    let source = Etcd::new(["http://127.0.0.1:1"], "myapp/db.json")
        .await
        .expect("the client is built lazily, so this succeeds");

    let error = source
        .fetch()
        .await
        .expect_err("nothing is listening there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(
        error.to_string().contains("etcd http://127.0.0.1:1"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Several keys as one document
// ---------------------------------------------------------------------------

use dynamic_config_etcd::Keys;

/// The rule a named list inherits from a list of `.file(..)` calls: call
/// order, and the later key wins.
#[tokio::test]
async fn named_keys_merge_in_call_order_and_the_later_one_wins() {
    let etcd = etcd_holding(&[
        (
            "myapp/base.json",
            r#"{"db": {"host": "base", "port": 5432}}"#,
        ),
        ("myapp/local.json", r#"{"db": {"port": 6432}}"#),
    ])
    .await;

    let source = Etcd::new(
        [etcd.endpoint.as_str()],
        Keys::several(["myapp/base.json", "myapp/local.json"]),
    )
    .await
    .expect("the container is up");

    let fetched = source.fetch().await.expect("both keys are there");

    let merged = dynamic_config::Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&dynamic_config::Value::String("base".to_owned())),
        "a key the later document never mentions survives"
    );
    assert_eq!(
        merged.get("db.port"),
        Some(&dynamic_config::Value::Integer(6432)),
        "and the later document wins where they meet"
    );
}

/// The other half of the same feature, through etcd's native range read: the
/// sections under a prefix become one document.
#[tokio::test]
async fn a_prefix_folds_its_sections_into_one_document() {
    let etcd = etcd_holding(&[
        ("myapp/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("myapp/server.json", r#"{"server": {"port": 8080}}"#),
        // Outside the prefix: proof the range end is where it should be.
        ("other/db.json", r#"{"db": {"host": "wrong"}}"#),
    ])
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("myapp/"))
        .await
        .expect("the container is up")
        .with_format(Format::Json);

    let fetched = source.fetch().await.expect("the range is readable");

    let merged = dynamic_config::Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&dynamic_config::Value::String("db.internal".to_owned()))
    );
    assert_eq!(
        merged.get("server.port"),
        Some(&dynamic_config::Value::Integer(8080))
    );
}

/// A prefix says "these are disjoint sections". Two of them supplying one path
/// is a deployment bug, so it is named rather than silently resolved — and the
/// report names paths, never values.
#[tokio::test]
async fn two_keys_under_a_prefix_supplying_one_path_are_refused_by_name() {
    let etcd = etcd_holding(&[
        (
            "clash/db.json",
            r#"{"db": {"password": "hunter2-from-first"}}"#,
        ),
        (
            "clash/extra.json",
            r#"{"db": {"password": "hunter2-from-second"}}"#,
        ),
    ])
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("clash/"))
        .await
        .unwrap()
        .with_format(Format::Json);

    let error = source
        .fetch()
        .await
        .expect_err("both keys supply db.password");

    let printed = format!("{error} {error:?}");

    assert!(printed.contains("clash/db.json"), "{printed}");
    assert!(printed.contains("clash/extra.json"), "{printed}");
    assert!(printed.contains("db.password"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a collision report names paths and never values: {printed}"
    );
}

/// The same keys named in an order are not a bug: naming them *is* saying
/// which one wins.
#[tokio::test]
async fn the_same_overlap_is_fine_once_the_caller_names_an_order() {
    let etcd = etcd_holding(&[
        ("ordered/db.json", r#"{"db": {"host": "first"}}"#),
        ("ordered/extra.json", r#"{"db": {"host": "second"}}"#),
    ])
    .await;

    let source = Etcd::new(
        [etcd.endpoint.as_str()],
        Keys::several(["ordered/db.json", "ordered/extra.json"]),
    )
    .await
    .unwrap();

    let fetched = source.fetch().await.expect("a named order resolves it");

    assert!(fetched.text.contains("second"), "{}", fetched.text);
}

/// Fail-whole, not merge-what-came-back. A configuration quietly missing a
/// section is worse than a refresh that failed: the last document keeps
/// serving either way, and only one of the two says so.
#[tokio::test]
async fn one_unreadable_key_fails_the_whole_fetch_and_names_it() {
    let etcd = etcd_holding(&[("partial/db.json", r#"{"db": {"host": "here"}}"#)]).await;

    let source = Etcd::new(
        [etcd.endpoint.as_str()],
        Keys::several(["partial/db.json", "partial/absent.json"]),
    )
    .await
    .unwrap();

    let error = source
        .fetch()
        .await
        .expect_err("one of the two keys is not there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("partial/absent.json"), "{error}");
    assert!(error.to_string().contains("holds no value"), "{error}");
}

/// A prefix is caller input and the answer to it is server input. The budget
/// is what stands between a mistyped prefix and a process that pulls a whole
/// cluster's key space into memory.
#[tokio::test]
async fn a_prefix_matching_more_keys_than_the_budget_is_refused() {
    let etcd = etcd_holding(&[("many/000000.json", "{}")]).await;

    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    // One over the budget. In chunks of a hundred, because etcd caps a
    // transaction at `--max-txn-ops` — 128 by default — which is the same
    // limit the source's own named-list read answers to.
    let puts: Vec<_> = (1..=512)
        .map(|n| etcd_client::TxnOp::put(format!("many/{n:06}.json"), "{}", None))
        .collect();

    for chunk in puts.chunks(100) {
        client
            .txn(etcd_client::Txn::new().and_then(chunk.to_vec()))
            .await
            .unwrap();
    }

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("many/"))
        .await
        .unwrap()
        .with_format(Format::Json);

    let error = source
        .fetch()
        .await
        .expect_err("513 keys is past the budget");

    assert!(error.to_string().contains("narrow the prefix"), "{error}");
}

/// The end of the feature: a merged document is a document, and loads like one.
#[tokio::test]
async fn a_merged_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
    }

    let etcd = etcd_holding(&[
        ("split/host.json", r#"{"db": {"host": "db.internal"}}"#),
        ("split/port.json", r#"{"db": {"port": 6432}}"#),
    ])
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("split/"))
        .await
        .unwrap()
        .with_format(Format::Json);

    let fetched = source.fetch().await.unwrap();

    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];
    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("two keys became one document");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
        }
    );

    // Provenance is store-grained from here on: one layer, so `source_of`
    // answers with the store and the set — which is why `describe()` names
    // every key it read.
    assert!(
        source.describe().contains("prefix split/"),
        "{}",
        source.describe()
    );
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

/// A change made after the watch is established reaches the callback.
#[tokio::test]
async fn a_change_reaches_the_callback() {
    let etcd = etcd_with("myapp/watched.json", r#"{"db": {"host": "first"}}"#).await;

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/watched.json")
        .await
        .unwrap();

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    // The watch is established asynchronously, so a write issued immediately
    // can land before the stream exists. Retried rather than slept through:
    // a fixed sleep is the kind of test that passes on a laptop and flakes in
    // CI.
    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    let document = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            client
                .put("myapp/watched.json", r#"{"db": {"host": "second"}}"#, None)
                .await
                .unwrap();

            if let Ok(Some(document)) =
                tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv()).await
            {
                return document;
            }
        }
    })
    .await
    .expect("the watch should deliver the change");

    assert_eq!(document.format, Format::Json);
    assert!(document.text.contains("second"), "{}", document.text);

    // Dropping the task is the whole cancellation story.
    watch.abort();
}

/// A deletion is not a configuration, so it is not reported.
#[tokio::test]
async fn a_deletion_is_not_reported_as_a_change() {
    let etcd = etcd_with("myapp/deleted.json", r#"{"db": {"host": "first"}}"#).await;

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/deleted.json")
        .await
        .unwrap();

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    // Establish the watch by proving a put gets through first.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            client
                .put("myapp/deleted.json", r#"{"db": {"host": "second"}}"#, None)
                .await
                .unwrap();

            if tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv())
                .await
                .is_ok()
            {
                return;
            }
        }
    })
    .await
    .expect("the watch should be running by now");

    client.delete("myapp/deleted.json", None).await.unwrap();

    let after = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv()).await;

    assert!(
        after.is_err(),
        "a deleted key must not be pushed as a configuration"
    );

    watch.abort();
}

/// The property a watch on a set exists to have, and the only reason the
/// prefix shape was built while the named list was refused: **every delivery
/// agrees with itself**.
///
/// One generation is stamped into both keys of the set and both are written in
/// one transaction, so every state the cluster is ever in has the two halves
/// equal. A watch that woke on one key's event and then re-read the set key by
/// key — or re-read it at "now" while a second transaction was landing — would
/// eventually deliver `db.generation` from one write beside `server.generation`
/// from another: a document that never existed at any revision. Reading the
/// range at the event's own revision is what makes that impossible, and this
/// is the test that would catch it going away.
#[tokio::test]
async fn a_prefix_watch_never_delivers_a_document_that_never_existed() {
    let etcd = etcd_holding(&[
        ("stamped/db.json", r#"{"db": {"generation": 0}}"#),
        ("stamped/server.json", r#"{"server": {"generation": 0}}"#),
    ])
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("stamped/"))
        .await
        .expect("the container is up")
        .with_format(Format::Json);

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document.text);
                Ok(())
            })
            .await
    });

    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    /// Writes both halves of the set at one revision, so a torn *write* can
    /// never be mistaken for the torn *read* this test is about.
    async fn stamp(client: &mut etcd_client::Client, generation: i64) {
        client
            .txn(etcd_client::Txn::new().and_then(vec![
                etcd_client::TxnOp::put(
                    "stamped/db.json",
                    format!(r#"{{"db": {{"generation": {generation}}}}}"#),
                    None,
                ),
                etcd_client::TxnOp::put(
                    "stamped/server.json",
                    format!(r#"{{"server": {{"generation": {generation}}}}}"#),
                    None,
                ),
            ]))
            .await
            .expect("the transaction should be accepted");
    }

    // The stream is established asynchronously, so writes are repeated until
    // one of them is delivered rather than slept through — a fixed sleep is
    // the kind of test that passes on a laptop and flakes in CI. Generation
    // zero throughout, so priming adds nothing the assertions have to know
    // about.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            stamp(&mut client, 0).await;

            if tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv())
                .await
                .is_ok()
            {
                return;
            }
        }
    })
    .await
    .expect("the watch should be running by now");

    for generation in 1..=6 {
        stamp(&mut client, generation).await;
    }

    let mut seen = 0;
    let mut highest = 0;

    while let Ok(Some(text)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv()).await
    {
        let tree = dynamic_config::Value::parse(&text, Format::Json)
            .expect("every delivery is a document");

        let db = tree.get("db.generation").cloned();
        let server = tree.get("server.generation").cloned();

        assert_eq!(
            db, server,
            "a delivery whose two halves disagree is a document that never \
             existed at any revision: {text}"
        );

        if let Some(dynamic_config::Value::Integer(generation)) = db {
            highest = highest.max(generation);
        }

        seen += 1;

        if highest == 6 {
            break;
        }
    }

    assert!(seen > 0, "the watcher must have delivered something");
    assert_eq!(highest, 6, "and must have caught up to the last write");

    watch.abort();
}

/// A key leaving the prefix changes the set, so it is reported — the delivered
/// document is simply the sections that are left. This is the one place the
/// prefix rule differs from the single-key one, where a deletion is no
/// configuration at all and is skipped.
#[tokio::test]
async fn a_deletion_under_a_prefix_is_a_change_to_the_set() {
    let etcd = etcd_holding(&[
        ("shrinking/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("shrinking/server.json", r#"{"server": {"port": 8080}}"#),
    ])
    .await;

    let source = Etcd::new([etcd.endpoint.as_str()], Keys::prefix("shrinking/"))
        .await
        .expect("the container is up")
        .with_format(Format::Json);

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document.text);
                Ok(())
            })
            .await
    });

    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    // Establish the stream the same way, on a third key that is deleted again
    // before the assertion — so the set under test is back to two.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            client
                .put("shrinking/probe.json", r#"{"probe": {"n": 1}}"#, None)
                .await
                .unwrap();

            if tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv())
                .await
                .is_ok()
            {
                return;
            }
        }
    })
    .await
    .expect("the watch should be running by now");

    client.delete("shrinking/probe.json", None).await.unwrap();

    let text = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
        .await
        .expect("a delete under the prefix is a change")
        .expect("the watch is still running");

    let tree = dynamic_config::Value::parse(&text, Format::Json).expect("it is a document");

    assert_eq!(tree.get("probe.n"), None, "the deleted section is gone");
    assert_eq!(
        tree.get("server.port"),
        Some(&dynamic_config::Value::Integer(8080)),
        "and the sections that remain are all there: {text}"
    );

    watch.abort();
}

// ---------------------------------------------------------------------------
// Authenticating, and surviving a token that expires
// ---------------------------------------------------------------------------

use dynamic_config_etcd::{Client, ConnectOptions};

/// Turns on etcd's authentication, with a `myapp` user that can read `myapp/`.
///
/// The auth token TTL is deliberately tiny: it is what makes the expiry path
/// reachable in a test rather than after five minutes of waiting.
async fn secured_etcd(key: &str, value: &str) -> Running {
    let container = start_resilient(|| {
        GenericImage::new(IMAGE, TAG)
            .with_exposed_port(2379.tcp())
            .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
            .with_cmd([
                "etcd",
                "--advertise-client-urls=http://0.0.0.0:2379",
                "--listen-client-urls=http://0.0.0.0:2379",
                // `simple` tokens carry a TTL; `jwt` would not expire this way.
                "--auth-token=simple,ttl=1s",
            ])
    })
    .await;

    let port = container.get_host_port_ipv4(2379).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");

    let mut client = Client::connect([endpoint.as_str()], None).await.unwrap();

    client.put(key, value, None).await.unwrap();

    // etcd refuses to enable auth without a root user.
    client.user_add("root", "rootpw", None).await.unwrap();
    client.user_add("myapp", "apppw", None).await.unwrap();
    client
        .role_add("reader")
        .await
        .expect("a role to hang the permission on");
    // A prefix permission: `read` plus a range end, which is how etcd spells
    // "everything under `myapp/`".
    client
        .role_grant_permission(
            "reader",
            etcd_client::Permission::read("myapp/").with_prefix(),
        )
        .await
        .unwrap();
    client.user_grant_role("myapp", "reader").await.unwrap();
    client.user_grant_role("root", "root").await.unwrap();
    client.auth_enable().await.expect("auth should turn on");

    Running {
        endpoint,
        _container: container,
    }
}

#[tokio::test]
async fn a_user_and_password_authenticate() {
    let etcd = secured_etcd("myapp/secured.json", r#"{"db": {"host": "secured"}}"#).await;

    let refused = Etcd::new([etcd.endpoint.as_str()], "myapp/secured.json")
        .await
        .unwrap()
        .fetch()
        .await;

    assert!(refused.is_err(), "auth is on, so an anonymous read fails");

    let source = Etcd::with_options(
        [etcd.endpoint.as_str()],
        "myapp/secured.json",
        ConnectOptions::new().with_user("myapp", "apppw"),
    )
    .await
    .expect("the credentials are good");

    let fetched = source.fetch().await.expect("and the key is readable");

    assert!(fetched.text.contains("secured"), "{}", fetched.text);
}

/// etcd's simple tokens expire. A configuration source outlives one, so it has
/// to notice and reconnect rather than start failing.
#[tokio::test]
async fn a_source_keeps_reading_past_the_life_of_its_auth_token() {
    let etcd = secured_etcd("myapp/expiring.json", r#"{"db": {"host": "still-here"}}"#).await;

    let source = Etcd::with_options(
        [etcd.endpoint.as_str()],
        "myapp/expiring.json",
        ConnectOptions::new().with_user("myapp", "apppw"),
    )
    .await
    .unwrap();

    assert!(
        source.fetch().await.is_ok(),
        "the first read uses a fresh token"
    );

    // Well past the one-second TTL.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let fetched = source
        .fetch()
        .await
        .expect("an expired token should be replaced, not reported");

    assert!(fetched.text.contains("still-here"), "{}", fetched.text);
}

#[tokio::test]
async fn a_source_can_share_a_client_the_program_already_has() {
    let etcd = etcd_with("myapp/shared.json", r#"{"db": {"host": "shared"}}"#).await;

    let client = Client::connect([etcd.endpoint.as_str()], None)
        .await
        .expect("the program's own client");

    let source = Etcd::from_client(client, "myapp/shared.json");

    let fetched = source.fetch().await.expect("the shared client reads fine");

    assert!(fetched.text.contains("shared"), "{}", fetched.text);
    assert!(
        source.describe().contains("an existing client"),
        "an error should not name endpoints this source never had: {}",
        source.describe()
    );
}

/// A shared client recovers from an expired token like any other: the
/// credentials live in the client, so there is nothing this source would have to
/// own in order to ask for a new one.
#[tokio::test]
async fn a_shared_client_also_survives_its_token_expiring() {
    let etcd = secured_etcd(
        "myapp/shared-auth.json",
        r#"{"db": {"host": "still-here"}}"#,
    )
    .await;

    let client = Client::connect(
        [etcd.endpoint.as_str()],
        Some(ConnectOptions::new().with_user("myapp", "apppw")),
    )
    .await
    .unwrap();

    let source = Etcd::from_client(client, "myapp/shared-auth.json");

    assert!(source.fetch().await.is_ok(), "the first read works");

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let fetched = source
        .fetch()
        .await
        .expect("a shared client can be asked for a new token");

    assert!(fetched.text.contains("still-here"), "{}", fetched.text);
}

// ---------------------------------------------------------------------------
// Reporting a watch that is failing
//
// The half of a store `dynamic-config` cannot see. A delivery keeps
// `RemoteStatus` current, so a *working* watch needs none of this; a stream
// that broke delivers nothing, and without `reporting_to` would say nothing
// either — `remote_up` describing the last delivery rather than the last
// attempt. The unit tests pin the wiring against a port nothing listens on;
// these need a real server, because the interesting case is a watch that was
// running when the store went away.
// ---------------------------------------------------------------------------

use dynamic_config::{Remote, RemoteSink};

/// A watch that was working, and a store that goes away under it. `up` has to
/// fall to zero *and* the last good read has to keep ageing: an alert wants
/// both halves, and a failure that reset the staleness clock would hide the
/// one that says how old the served document is.
#[tokio::test]
async fn a_broken_stream_reports_the_store_as_unreachable_and_leaves_the_last_read_ageing() {
    // Its own `static`, because a `RemoteSink` needs one and two tests sharing
    // one would race.
    static WATCHED: Remote = Remote::new();

    fn reloaded() -> Result<(), dynamic_config::Error> {
        Ok(())
    }

    let etcd = etcd_with("myapp/reported.json", r#"{"db": {"host": "first"}}"#).await;

    // A real pull first, so `last_fetch` holds something the failures that
    // follow must not disturb.
    WATCHED.set_async(
        Etcd::new([etcd.endpoint.as_str()], "myapp/reported.json")
            .await
            .unwrap(),
    );
    WATCHED.refresh_async().await.expect("the key is there");

    let before = WATCHED.status();

    assert_eq!(before.reachable(), Some(true), "the store just answered");
    assert!(before.last_fetch.is_some());

    let source = Etcd::new([etcd.endpoint.as_str()], "myapp/reported.json")
        .await
        .unwrap()
        .reporting_to(RemoteSink::new(&WATCHED, reloaded, "etcd"));

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    let mut client = etcd_client::Client::connect([etcd.endpoint.as_str()], None)
        .await
        .unwrap();

    // Establish the watch by proving a put gets through first: breaking a
    // stream that does not exist yet would prove nothing about a stream.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            client
                .put("myapp/reported.json", r#"{"db": {"host": "second"}}"#, None)
                .await
                .unwrap();

            if tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv())
                .await
                .is_ok()
            {
                return;
            }
        }
    })
    .await
    .expect("the watch should be running by now");

    // The store goes away under a running watch — a killed member, a network
    // that stopped being one. This is the state that used to be invisible.
    etcd._container
        .stop_with_timeout(Some(0))
        .await
        .expect("the container should stop");

    let error = tokio::time::timeout(std::time::Duration::from_secs(60), watch)
        .await
        .expect("the watch should end when the store does")
        .expect("the watch task itself must not panic")
        .expect_err("a watch never returns `Ok`");

    let after = WATCHED.status();

    assert_eq!(
        after.reachable(),
        Some(false),
        "a watch that cannot reach its store is a store that is not \
         answering, and nothing else was going to say so: {error}"
    );
    assert!(after.consecutive_failures >= 1, "{after:?}");
    assert_eq!(
        after.last_fetch, before.last_fetch,
        "the last *good* read has to keep ageing, or the staleness alert \
         resets every time the store fails"
    );
    assert_eq!(
        after.fetches, before.fetches,
        "an attempt that returned nothing is not a fetch"
    );
    assert!(
        !format!("{after:?}").contains("127.0.0.1"),
        "a store's address never enters a status: {after:?}"
    );
}

/// The one failure a watch does **not** report: a token that expired and was
/// replaced. etcd's simple tokens carry a TTL of five minutes by default, so a
/// long-lived watch turns one over routinely — and because only a delivery or
/// a fetch clears the streak, reporting a recovery would pin `remote_up` at
/// zero on a perfectly healthy cluster until the next configuration change.
/// Only the cure *failing* is worth waking somebody for.
#[tokio::test]
async fn a_watch_that_replaces_its_expired_token_reports_nothing() {
    static REAUTHED: Remote = Remote::new();

    fn reloaded() -> Result<(), dynamic_config::Error> {
        Ok(())
    }

    // The TTL here is one second, which is what makes the expiry reachable in
    // a test rather than after five minutes of waiting.
    let etcd = secured_etcd("myapp/reauth.json", r#"{"db": {"host": "first"}}"#).await;

    let credentials = || ConnectOptions::new().with_user("myapp", "apppw");

    REAUTHED.set_async(
        Etcd::with_options([etcd.endpoint.as_str()], "myapp/reauth.json", credentials())
            .await
            .unwrap(),
    );
    REAUTHED
        .refresh_async()
        .await
        .expect("the credentials are good");

    let source = Etcd::with_options([etcd.endpoint.as_str()], "myapp/reauth.json", credentials())
        .await
        .unwrap()
        .reporting_to(RemoteSink::new(&REAUTHED, reloaded, "etcd"));

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let watch = tokio::spawn(async move {
        source
            .watch(move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    // `myapp` may only read, so the writes come from root. A fresh connection
    // per write rather than one client held across the sleep below: root's
    // token carries the same one-second TTL, and a test helper that has to
    // recover from expiry would be testing the helper.
    let write = |value: &'static str| {
        let endpoint = etcd.endpoint.clone();

        async move {
            Client::connect(
                [endpoint.as_str()],
                Some(ConnectOptions::new().with_user("root", "rootpw")),
            )
            .await
            .expect("root's credentials are good")
            .put("myapp/reauth.json", value, None)
            .await
            .expect("root may write");
        }
    };

    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            write(r#"{"db": {"host": "second"}}"#).await;

            if tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv())
                .await
                .is_ok()
            {
                return;
            }
        }
    })
    .await
    .expect("the watch should be running by now");

    // Well past the one-second TTL: whatever the stream does next, it does it
    // with a token the server has forgotten.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let document = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            write(r#"{"db": {"host": "third"}}"#).await;

            if let Ok(Some(document)) =
                tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv()).await
            {
                return document;
            }
        }
    })
    .await
    .expect("an expired token should be replaced, not reported");

    assert!(document.text.contains("third"), "{}", document.text);

    let status = REAUTHED.status();

    assert_eq!(
        status.consecutive_failures, 0,
        "a re-authentication that worked is not a failure an operator needs \
         woken for; only one that fails is: {status:?}"
    );
    assert_eq!(status.reachable(), Some(true));

    watch.abort();
}
