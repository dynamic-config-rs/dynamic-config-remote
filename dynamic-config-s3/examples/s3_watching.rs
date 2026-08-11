//! S3 (here: MinIO), watched by polling the object's ETag.
//!
//! ```text
//! docker run --rm -p 9000:9000 quay.io/minio/minio server /data
//!
//! # Make a bucket and put a document in it (mc lives in its own image):
//! docker run --rm --network host --entrypoint sh quay.io/minio/mc -c '
//!   mc alias set local http://127.0.0.1:9000 minioadmin minioadmin &&
//!   mc mb local/myapp-config &&
//!   echo "{\"db\": {\"host\": \"db.internal\", \"port\": 5432}}" \
//!     | mc pipe local/myapp-config/prod/db.json'
//!
//! cargo run -p dynamic-config-s3 --example s3_watching
//! ```
//!
//! Each tick is a `HEAD`; only a changed ETag costs a `GET` — polling a
//! bucket that charges per gigabyte should not mean downloading the object
//! to learn it has not changed.

use std::time::Duration;

use dynamic_config::{dynamic_config, AsyncRemoteSource, RemoteWatch};
use dynamic_config_s3::S3;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

async fn source(endpoint: &str) -> S3 {
    // The endpoint override is what makes every S3-compatible server work;
    // against real AWS, drop it and let the environment supply everything.
    let config = aws_config::from_env()
        .endpoint_url(endpoint)
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "minioadmin",
            "minioadmin",
            None,
            None,
            "example",
        ))
        .load()
        .await;

    S3::with_config(&config, "myapp-config", "prod/db.json")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".to_owned());

    let reader = source(&endpoint).await;

    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote_async(reader);
    DbConfig::refresh_remote_async().await?;

    // No files: the fetched document is the whole configuration. Initializing
    // through the builder is also what lets the sink from `remote_sink()` reload later.
    DbConfig::builder("db").env("APP_").init_async().await?;

    // The sink is taken here, at wiring: it remembers which source is
    // installed, and a sink whose source is later replaced refuses to push.
    let sink = DbConfig::remote_sink();

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: a future, cancelled by dropping it. The interval is the
    // trade between freshness and request count.
    // ---------------------------------------------------------------------
    println!("\nwatching for 30 seconds — try another `mc pipe ...`");

    let watcher = source(&endpoint).await;
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let task = tokio::spawn(async move {
        watcher
            .watch(&watching, Duration::from_secs(2), move |document| {
                sink.apply(document)
            })
            .await
    });

    tokio::time::sleep(Duration::from_secs(30)).await;

    watch.stop();
    let _ = task.await;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
