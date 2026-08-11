//! Against a real NATS server, in a container.
//!
//! ```text
//! cargo test -p dynamic-config-nats
//! ```
//!
//! Needs a working Docker daemon. The server is started with `--jetstream`,
//! because a key/value bucket is a JetStream feature and a plain server has
//! none.

use async_nats::jetstream::kv::Config;
use dynamic_config::{AsyncRemoteSource, Format};
use dynamic_config_nats::Nats;
use testcontainers::ImageExt;
use testcontainers_modules::nats::{Nats as NatsImage, NatsServerCmd};

const BUCKET: &str = "config";

struct Running {
    server: String,
    store: async_nats::jetstream::kv::Store,
    _container: testcontainers::ContainerAsync<NatsImage>,
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

/// A server with a bucket, holding `key` if one is given.
async fn nats_with(entry: Option<(&str, &str)>) -> Running {
    let command = NatsServerCmd::default().with_jetstream();

    let container = start_resilient(|| NatsImage::default().with_cmd(&command)).await;

    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("NATS should expose its client port");
    let server = format!("nats://127.0.0.1:{port}");

    let client = async_nats::connect(&server)
        .await
        .expect("the container should accept a connection");

    let store = async_nats::jetstream::new(client)
        .create_key_value(Config {
            bucket: BUCKET.to_owned(),
            ..Config::default()
        })
        .await
        .expect("JetStream is enabled, so the bucket should be creatable");

    if let Some((key, value)) = entry {
        store
            .put(key, value.to_owned().into())
            .await
            .expect("writing the key should succeed");
    }

    Running {
        server,
        store,
        _container: container,
    }
}

#[tokio::test]
async fn a_key_holds_a_whole_configuration_document() {
    let nats = nats_with(Some((
        "db.json",
        r#"{"db": {"host": "localhost", "port": 5432}}"#,
    )))
    .await;

    let source = Nats::new(&nats.server, BUCKET, "db.json")
        .await
        .expect("the bucket exists");

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

    let nats = nats_with(Some((
        "loads.json",
        r#"{"db": {"host": "db.internal", "port": 6432}}"#,
    )))
    .await;

    let source = Nats::new(&nats.server, BUCKET, "loads.json").await.unwrap();
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
    let nats = nats_with(None).await;

    let source = Nats::new(&nats.server, BUCKET, "absent.json")
        .await
        .unwrap();

    let error = source.fetch().await.expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("holds no value"), "{error}");
}

/// The bucket is not created here on purpose — a configuration reader that
/// provisioned storage would hide a misconfigured deployment behind an empty
/// one, so a missing bucket has to be loud.
#[tokio::test]
async fn a_bucket_that_does_not_exist_fails_at_construction() {
    let nats = nats_with(None).await;

    let error = Nats::new(&nats.server, "no-such-bucket", "db.json")
        .await
        .expect_err("the bucket was never created");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("no-such-bucket"), "{error}");
}

#[tokio::test]
async fn an_unreachable_server_fails_at_construction() {
    // Port 1 is reserved and nothing serves NATS on it. Unlike a gRPC client,
    // this one connects eagerly, so the failure lands here rather than at fetch.
    let error = Nats::new("nats://127.0.0.1:1", BUCKET, "db.json")
        .await
        .expect_err("nothing is listening there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
}

#[tokio::test]
async fn a_key_naming_no_format_says_which_call_fixes_it() {
    let nats = nats_with(Some(("plain", r#"{"db": {"host": "a"}}"#))).await;

    let source = Nats::new(&nats.server, BUCKET, "plain").await.unwrap();

    let error = source.fetch().await.expect_err("no format is known");

    assert!(error.to_string().contains("with_format"), "{error}");

    // And with the format supplied, the same key reads fine.
    let source = Nats::new(&nats.server, BUCKET, "plain")
        .await
        .unwrap()
        .with_format(Format::Json);

    assert_eq!(source.fetch().await.unwrap().format, Format::Json);
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

/// A change made after the watch is established reaches the callback.
#[tokio::test]
async fn a_change_reaches_the_callback() {
    let nats = nats_with(Some(("watched.json", r#"{"db": {"host": "first"}}"#))).await;

    let source = Nats::new(&nats.server, BUCKET, "watched.json")
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

    // Written in a retry loop rather than after a fixed sleep: the watch is
    // established asynchronously, and a sleep long enough to be reliable in CI
    // is a sleep too long to want in a test.
    let document = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            nats.store
                .put("watched.json", r#"{"db": {"host": "second"}}"#.into())
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

/// A delete is an operation on the bucket, not a configuration.
#[tokio::test]
async fn a_delete_is_not_reported_as_a_change() {
    let nats = nats_with(Some(("deleted.json", r#"{"db": {"host": "first"}}"#))).await;

    let source = Nats::new(&nats.server, BUCKET, "deleted.json")
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

    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            nats.store
                .put("deleted.json", r#"{"db": {"host": "second"}}"#.into())
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

    nats.store.delete("deleted.json").await.unwrap();

    let after = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv()).await;

    assert!(
        after.is_err(),
        "a deleted key must not be pushed as a configuration"
    );

    watch.abort();
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

use dynamic_config_nats::ConnectOptions;

#[tokio::test]
async fn a_user_and_password_authenticate() {
    let command = NatsServerCmd::default()
        .with_jetstream()
        .with_user("myapp")
        .with_password("hunter2");

    let container = start_resilient(|| NatsImage::default().with_cmd(&command)).await;

    let port = container.get_host_port_ipv4(4222).await.unwrap();
    let server = format!("nats://127.0.0.1:{port}");

    let refused = Nats::new(&server, BUCKET, "db.json").await;

    assert!(refused.is_err(), "the server requires credentials");

    let client = ConnectOptions::with_user_and_password("myapp".into(), "hunter2".into())
        .connect(&server)
        .await
        .expect("the credentials are good");

    async_nats::jetstream::new(client.clone())
        .create_key_value(Config {
            bucket: BUCKET.to_owned(),
            ..Config::default()
        })
        .await
        .unwrap()
        .put("db.json", r#"{"db": {"host": "authenticated"}}"#.into())
        .await
        .unwrap();

    let source = Nats::with_options(
        &server,
        BUCKET,
        "db.json",
        ConnectOptions::with_user_and_password("myapp".into(), "hunter2".into()),
    )
    .await
    .expect("with credentials it connects");

    let fetched = source.fetch().await.expect("and reads");

    assert!(fetched.text.contains("authenticated"), "{}", fetched.text);
}

#[tokio::test]
async fn a_source_can_share_a_client_the_program_already_has() {
    let nats = nats_with(Some(("shared.json", r#"{"db": {"host": "shared"}}"#))).await;

    let client = async_nats::connect(&nats.server)
        .await
        .expect("the program's own client");

    let source = Nats::from_client(client, BUCKET, "shared.json")
        .await
        .expect("the bucket is there");

    let fetched = source.fetch().await.expect("the shared client reads fine");

    assert!(fetched.text.contains("shared"), "{}", fetched.text);
    assert!(
        source.describe().contains("an existing connection"),
        "an error should not name a server this source never dialled: {}",
        source.describe()
    );
}
