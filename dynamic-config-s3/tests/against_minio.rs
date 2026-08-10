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
use dynamic_config_s3::{Client, S3};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::minio::MinIO;

const BUCKET: &str = "config";

struct Running {
    client: Client,
    _container: testcontainers::ContainerAsync<MinIO>,
}

/// Starts MinIO, makes a bucket, and puts one object in it.
async fn minio_with(key: &str, body: &str) -> Running {
    let container = MinIO::default()
        .start()
        .await
        .expect("Docker should be available; these tests exercise a real S3 API");

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
