//! Redis, watched with keyspace notifications — change-driven, no polling.
//!
//! ```text
//! docker run --rm -p 6379:6379 redis:7 --notify-keyspace-events KEA
//!
//! docker exec -i $(docker ps -q -f ancestor=redis:7) \
//!   redis-cli set myapp/db.json '{"db": {"host": "db.internal", "port": 5432}}'
//!
//! cargo run -p dynamic-config-redis --example redis_watching
//! ```
//!
//! The `--notify-keyspace-events KEA` flag matters: notifications are off by
//! default, and the watch checks for them at startup and refuses to run
//! silently without them.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_redis::Redis;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned());

    // Credentials live in the URL, where Redis puts them:
    //
    //     redis://user:password@host:6379/0
    //
    // and never reach an error message — the URL is redacted first.
    let reader = Redis::new(&url, "myapp/db.json")?;

    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote(reader);
    DbConfig::refresh_remote()?;

    // No files: the fetched document is the whole configuration. Initializing
    // through the builder is also what lets the sink from `remote_sink()` reload later.
    DbConfig::builder("db").env("APP_").init()?;

    // The sink is taken here, at wiring: it remembers which source is
    // installed, and a sink whose source is later replaced refuses to push.
    let sink = DbConfig::remote_sink();

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: blocking, so it lives on a thread. The `Watching` token is
    // how the thread learns it is time to stop.
    // ---------------------------------------------------------------------
    println!("\nwatching for 30 seconds — try another `redis-cli set ...`");

    // `reporting_to` is the other half of the sink: `apply` records a delivery,
    // and this records an attempt that came back with nothing — so a loop whose
    // subscription died an hour ago stops reporting the server as healthy.
    let watcher = Redis::new(&url, "myapp/db.json")?.reporting_to(sink);
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || {
        // A watch that outlives its usefulness is stopped from outside;
        // a bad document should be logged and survived, not returned.
        watcher.watch(&watching, |document| {
            sink.apply(document)
                .map_err(|error| {
                    eprintln!("a document did not apply: {error}");
                    error
                })
                .or(Ok(()))
        })
    });

    std::thread::sleep(Duration::from_secs(30));

    // `stop()` — or dropping `watch` — ends the loop within a quarter second.
    watch.stop();
    thread.join().expect("the watch thread should end")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
