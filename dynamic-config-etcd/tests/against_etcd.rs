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
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

/// quay.io rather than Docker Hub: no pull limits, no anonymous rate limiting.
const IMAGE: &str = "quay.io/coreos/etcd";
const TAG: &str = "v3.5.17";

struct Running {
    endpoint: String,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

async fn etcd_with(key: &str, value: &str) -> Running {
    let container = GenericImage::new(IMAGE, TAG)
        .with_exposed_port(2379.tcp())
        .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
        .with_cmd([
            "etcd",
            "--advertise-client-urls=http://0.0.0.0:2379",
            "--listen-client-urls=http://0.0.0.0:2379",
        ])
        .start()
        .await
        .expect("Docker should be available; these tests exercise a real etcd");

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

// ---------------------------------------------------------------------------
// Authenticating, and surviving a token that expires
// ---------------------------------------------------------------------------

use dynamic_config_etcd::{Client, ConnectOptions};

/// Turns on etcd's authentication, with a `myapp` user that can read `myapp/`.
///
/// The auth token TTL is deliberately tiny: it is what makes the expiry path
/// reachable in a test rather than after five minutes of waiting.
async fn secured_etcd(key: &str, value: &str) -> Running {
    let container = GenericImage::new(IMAGE, TAG)
        .with_exposed_port(2379.tcp())
        .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
        .with_cmd([
            "etcd",
            "--advertise-client-urls=http://0.0.0.0:2379",
            "--listen-client-urls=http://0.0.0.0:2379",
            // `simple` tokens carry a TTL; `jwt` would not expire this way.
            "--auth-token=simple,ttl=1s",
        ])
        .start()
        .await
        .expect("Docker should be available");

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
