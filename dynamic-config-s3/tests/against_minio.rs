//! Against a real S3 API, in a container.
//!
//! MinIO rather than AWS: it speaks the same API, runs offline, and costs
//! nothing — and the point of these tests is the API, not the vendor. That the
//! crate works against MinIO at all is itself the assertion that matters for
//! everyone using Ceph, R2 or B2.
//!
//! ```text
//! cargo test -p dynamic-config-s3
//! ```

use std::time::Duration;

use dynamic_config::{AsyncRemoteSource, Format, RemoteWatch};
use dynamic_config_s3::{Client, Keys, S3};
use testcontainers_modules::minio::MinIO;

const BUCKET: &str = "config";

struct Running {
    client: Client,
    _container: testcontainers::ContainerAsync<MinIO>,
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

/// Starts MinIO, makes a bucket, and puts one object in it.
async fn minio_with(key: &str, body: &str) -> Running {
    let container = start_resilient(MinIO::default).await;

    let port = container.get_host_port_ipv4(9000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");

    // The credentials the module's image starts with.
    let config = aws_config::from_env()
        .endpoint_url(&endpoint)
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "minioadmin",
            "minioadmin",
            None,
            None,
            "tests",
        ))
        .load()
        .await;

    let s3 = aws_sdk_s3::config::Builder::from(&config)
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3);

    client
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("the bucket should be creatable");

    put(&client, key, body).await;

    Running {
        client,
        _container: container,
    }
}

async fn put(client: &Client, key: &str, body: &str) {
    client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .body(body.as_bytes().to_vec().into())
        .send()
        .await
        .expect("writing the object should succeed");
}

#[tokio::test]
async fn an_object_holds_a_whole_configuration_document() {
    let minio = minio_with("db.json", r#"{"db": {"host": "localhost", "port": 5432}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "db.json");
    let fetched = source.fetch().await.expect("the object is there");

    assert_eq!(fetched.format, Format::Json, "from the key's extension");
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

    let minio = minio_with(
        "loads.json",
        r#"{"db": {"host": "db.internal", "port": 6432}}"#,
    )
    .await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "loads.json");
    let fetched = source.fetch().await.unwrap();

    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];
    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("the object holds a whole document");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432
        }
    );
}

/// The rule a list of keys inherits from a list of files: call order, and the
/// later key wins. One `GetObject` per key, because S3 has no batch read.
#[tokio::test]
async fn several_keys_merge_in_call_order_and_the_later_key_wins() {
    let minio = minio_with(
        "prod/base.json",
        r#"{"db": {"host": "shared", "port": 5432}}"#,
    )
    .await;
    put(
        &minio.client,
        "prod/local.json",
        r#"{"db": {"host": "override"}}"#,
    )
    .await;

    let source = S3::from_client(
        minio.client.clone(),
        BUCKET,
        Keys::several(["prod/base.json", "prod/local.json"]),
    );

    let fetched = source.fetch().await.expect("both objects are there");

    assert_eq!(fetched.format, Format::Json);
    assert!(
        fetched.text.contains("override") && !fetched.text.contains("shared"),
        "the later key wins: {}",
        fetched.text
    );
    assert!(
        fetched.text.contains("5432"),
        "a value only the earlier key has survives: {}",
        fetched.text
    );
}

/// A prefix is the sections of a configuration, and `ListObjectsV2` finds
/// them. The "folder" object a console leaves behind is not one of them.
#[tokio::test]
async fn a_prefix_folds_every_object_under_it_into_one_document() {
    let minio = minio_with("prod/db.json", r#"{"db": {"host": "localhost"}}"#).await;
    put(
        &minio.client,
        "prod/server.json",
        r#"{"server": {"port": 8080}}"#,
    )
    .await;
    // The zero-byte object a console makes when somebody creates a folder.
    put(&minio.client, "prod/", "").await;
    // And one outside the prefix, which must not be dragged in.
    put(
        &minio.client,
        "staging/db.json",
        r#"{"db": {"host": "elsewhere"}}"#,
    )
    .await;

    let source = S3::from_client(minio.client.clone(), BUCKET, Keys::prefix("prod/"))
        .with_format(Format::Json);

    let fetched = source
        .fetch()
        .await
        .expect("the prefix matches two documents");

    assert!(fetched.text.contains("localhost"), "{}", fetched.text);
    assert!(fetched.text.contains("8080"), "{}", fetched.text);
    assert!(
        !fetched.text.contains("elsewhere"),
        "a key outside the prefix is not part of the set: {}",
        fetched.text
    );
}

/// Under a prefix nobody wrote an order, so two objects supplying one path is
/// a deployment bug — reported, naming both keys and the path, never a value.
#[tokio::test]
async fn two_objects_under_a_prefix_supplying_one_path_is_refused() {
    let minio = minio_with("clash/db.json", r#"{"db": {"password": "hunter2-left"}}"#).await;
    put(
        &minio.client,
        "clash/extra.json",
        r#"{"db": {"password": "hunter2-right"}}"#,
    )
    .await;

    let source = S3::from_client(minio.client.clone(), BUCKET, Keys::prefix("clash/"))
        .with_format(Format::Json);

    let error = source
        .fetch()
        .await
        .expect_err("both objects supply db.password");

    let printed = format!("{error} {error:?}");

    assert!(printed.contains("clash/db.json"), "{printed}");
    assert!(printed.contains("clash/extra.json"), "{printed}");
    assert!(printed.contains("db.password"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a collision report names paths and never values: {printed}"
    );
}

/// A prefix that matches nothing is a missing configuration rather than an
/// empty one.
#[tokio::test]
async fn a_prefix_that_matches_nothing_says_so() {
    let minio = minio_with("prod/db.json", r#"{"db": {"host": "a"}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, Keys::prefix("nowhere/"))
        .with_format(Format::Json);

    let error = source.fetch().await.expect_err("nothing is under it");

    assert!(error.to_string().contains("nothing matched"), "{error}");
}

/// One unreadable key fails the whole fetch, naming it: a configuration
/// quietly missing a section is worse than a refresh that failed.
#[tokio::test]
async fn one_missing_key_in_a_list_fails_the_whole_fetch() {
    let minio = minio_with("prod/present.json", r#"{"db": {"host": "a"}}"#).await;

    let source = S3::from_client(
        minio.client.clone(),
        BUCKET,
        Keys::several(["prod/present.json", "prod/absent.json"]),
    );

    let error = source.fetch().await.expect_err("the second key is absent");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("prod/absent.json"), "{error}");
}

#[tokio::test]
async fn an_object_that_is_not_there_is_a_remote_error() {
    let minio = minio_with("present.json", r#"{"db": {}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "absent.json");
    let error = source.fetch().await.expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
}

#[tokio::test]
async fn a_key_naming_no_format_says_which_call_fixes_it() {
    let minio = minio_with("plain", r#"{"db": {"host": "a"}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "plain");
    let error = source.fetch().await.expect_err("no format is known");

    assert!(error.to_string().contains("with_format"), "{error}");

    let source = S3::from_client(minio.client.clone(), BUCKET, "plain").with_format(Format::Json);

    assert_eq!(source.fetch().await.unwrap().format, Format::Json);
}

/// A new object body changes the ETag, which is what the watch is looking at.
#[tokio::test]
async fn a_change_reaches_the_callback() {
    let minio = minio_with("watched.json", r#"{"db": {"host": "first"}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "watched.json");
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let task = tokio::spawn(async move {
        source
            .watch(&watching, Duration::from_millis(200), move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    // The first tick records the ETag without firing, so writing into that
    // window would prime the loop with the new object.
    tokio::time::sleep(Duration::from_millis(700)).await;

    put(
        &minio.client,
        "watched.json",
        r#"{"db": {"host": "second"}}"#,
    )
    .await;

    let document = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("an ETag change should be noticed")
        .expect("the channel is open");

    assert!(document.text.contains("second"), "{}", document.text);

    watch.stop();
    let _ = task.await;
}

/// The object present at startup is not a change, so it is not reported.
#[tokio::test]
async fn the_starting_object_is_not_announced() {
    let minio = minio_with("quiet.json", r#"{"db": {"host": "first"}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "quiet.json");
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let task = tokio::spawn(async move {
        source
            .watch(&watching, Duration::from_millis(200), move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    let quiet = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await;

    assert!(
        quiet.is_err(),
        "a watch reports changes; announcing the current value would make every \
         restart look like an edit"
    );

    watch.stop();
    let _ = task.await;
}

/// A watch with no format refuses at the start rather than polling forever.
#[tokio::test]
async fn a_watch_with_no_format_refuses_at_the_start() {
    let minio = minio_with("plain-watched", r#"{"db": {}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "plain-watched");
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(200), |_| Ok(()))
        .await
        .expect_err("a watch that cannot parse what it fetches must not start");

    assert!(error.to_string().contains("with_format"), "{error}");
}

/// A deleted object is not a configuration change: the running snapshot
/// stays, and the watch stays alive for the object's return.
#[tokio::test]
async fn a_deleted_object_is_not_reported_as_a_change() {
    let minio = minio_with("doomed.json", r#"{"db": {"host": "first"}}"#).await;

    let source = S3::from_client(minio.client.clone(), BUCKET, "doomed.json");
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let task = tokio::spawn(async move {
        source
            .watch(&watching, Duration::from_millis(200), move |document| {
                let _ = sender.send(document);
                Ok(())
            })
            .await
    });

    // Let the first tick record the ETag, then delete the object.
    tokio::time::sleep(Duration::from_millis(700)).await;

    minio
        .client
        .delete_object()
        .bucket(BUCKET)
        .key("doomed.json")
        .send()
        .await
        .expect("deleting the object should succeed");

    let quiet = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await;
    assert!(
        quiet.is_err(),
        "no configuration is not a configuration; a deletion must not reach the callback"
    );

    // The watch survives the deletion: the object coming back IS a change.
    put(&minio.client, "doomed.json", r#"{"db": {"host": "back"}}"#).await;

    let document = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("the object's return should be noticed")
        .expect("the channel is open");

    assert!(document.text.contains("back"), "{}", document.text);

    watch.stop();
    let _ = task.await;
}

/// A server nobody is running is an error that names the endpoint, delivered
/// promptly — not a hang.
#[tokio::test]
async fn an_unreachable_server_is_a_prompt_error_naming_the_endpoint() {
    let config = aws_config::from_env()
        .endpoint_url("http://127.0.0.1:9")
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "nobody", "nothing", None, None, "tests",
        ))
        .load()
        .await;

    let source = S3::with_config(&config, BUCKET, "config.json");

    let error = tokio::time::timeout(Duration::from_secs(30), source.fetch())
        .await
        .expect("an unreachable server should fail, not hang")
        .expect_err("nothing listens on port 9");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}

// ---------------------------------------------------------------------------
// Reporting a failing watch
//
// One `#[dynamic_config]` type per test: the snapshot, the remote slot and the
// sink's generation all live in statics keyed by the type, so two tests
// sharing one would race and — worse — pass alone.
// ---------------------------------------------------------------------------

/// The failure nobody notices: a poll loop *survives* its failures by design,
/// so a bucket that stopped answering an hour ago looks exactly like a
/// configuration nobody has changed for an hour. Until this landed the status
/// said the store was fine, because the last thing it heard about was a
/// delivery.
///
/// The store is stopped rather than mocked: a poll failing against a real
/// endpoint that has gone away is the failure this is for, and the assertion
/// is a *pair* — `reachable()` goes to `Some(false)` while `last_fetch` keeps
/// the instant the last document really arrived, so an alert can ask "down,
/// and stale for how long".
#[tokio::test]
async fn a_poll_that_cannot_reach_the_bucket_reports_it_and_leaves_the_clock_running() {
    use dynamic_config::dynamic_config;

    #[dynamic_config]
    #[derive(Debug, serde::Deserialize)]
    struct Polled {
        // Never read: this test is about the status the store records, not
        // about the document.
        #[allow(dead_code)]
        host: String,
    }

    let minio = minio_with("polled.json", r#"{"db": {"host": "first"}}"#).await;

    Polled::set_remote_async(S3::from_client(minio.client.clone(), BUCKET, "polled.json"));
    Polled::refresh_remote_async()
        .await
        .expect("the store answers the first read");

    // Taken after the source is installed, which is what fences it.
    let sink = Polled::remote_sink();
    let before = sink.status();

    assert_eq!(before.reachable(), Some(true), "one fetch, and it answered");
    assert!(before.last_fetch.is_some());

    let watcher = S3::from_client(minio.client.clone(), BUCKET, "polled.json").reporting_to(sink);
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let polling = tokio::spawn(async move {
        watcher
            .watch(&watching, Duration::from_millis(200), |_| Ok(()))
            .await
    });

    // Let the first tick record the ETag, so the loop is genuinely watching
    // rather than starting up when the store goes away.
    tokio::time::sleep(Duration::from_millis(700)).await;

    minio
        ._container
        .stop_with_timeout(Some(0))
        .await
        .expect("the container should stop");

    let deadline = std::time::Instant::now() + Duration::from_secs(60);

    while sink.status().consecutive_failures == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let after = sink.status();

    assert!(
        !polling.is_finished(),
        "a failed check does not end the watch — which is exactly why \
         reporting it is the only way anyone hears about it"
    );
    assert_eq!(
        after.reachable(),
        Some(false),
        "a loop polling into the void is a store that is down"
    );
    assert_eq!(
        after.last_fetch, before.last_fetch,
        "the staleness clock keeps running: `last_fetch` is when a document \
         last arrived, and a failed attempt is not one"
    );
    assert_eq!(
        after.fetches, before.fetches,
        "a failure is not a fetch, however it is counted elsewhere"
    );
    assert_eq!(
        after
            .last_failure
            .as_ref()
            .expect("a failure was recorded")
            .kind,
        dynamic_config::ErrorKind::Remote,
        "a bucket that went away may yet come back"
    );

    watch.stop();
    let _ = polling.await;
}
