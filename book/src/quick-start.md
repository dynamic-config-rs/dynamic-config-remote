# Quick Start

One store, end to end: etcd holds the overrides, a file holds the base,
and a change in the cluster reaches a running process.

```toml
[dependencies]
dynamic-config = { version = "<version>", features = ["toml", "json", "watch"] }
dynamic-config-etcd = "<version>"
```

```rust,ignore
use std::time::Duration;

use dynamic_config::dynamic_config;
use dynamic_config_etcd::Etcd;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct AppConfig {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Etcd::new(["http://etcd:2379"]).key("myapp/config.json");

    let builder = AppConfig::builder("app")
        .file("config.toml")   // the base, from disk — later wins
        .remote(store);        // what the cluster says, on top

    builder.init()?;           // one fetch, fail fast if neither loads

    // Poll the store on its own cadence; file watching is separate and
    // both can run at once.
    AppConfig::refresh_remote(Duration::from_secs(15))?;

    println!("{}:{}", AppConfig::current().host, AppConfig::current().port);

    Ok(())
}
```

Try it without a cluster: `docker run -p 2379:2379 quay.io/coreos/etcd`
and `etcdctl put myapp/config.json '{"app": {"port": 9000}}'` — fifteen
seconds later, `current()` answers 9000.

Three rules carry from here to every store in this book:

1. **A store hands over a document; the engine does the rest.** Layering,
   validation, `explain`, the last-known-good cache — all identical to a
   file source, which is why a store outage is survivable:
   [the LKG chapter](https://dynamic-config-rs.github.io/last-known-good.html)
   belongs to the engine and applies unchanged.
2. **The key's extension names the format** — `config.json` is JSON,
   `config.properties` is properties — and `with_format` exists for keys
   that name nothing.
3. **Push stores watch, pull stores poll.** etcd, Consul, NATS and Redis
   can push (`watch_remote`); S3, Firestore and git poll; the
   [at-a-glance table](remote-stores/store-crates.md) says which is which.

From here: [Remote Stores](remote-stores.md) for the shared machinery,
your store's own chapter for its auth and its shape, and
[The Config Server](config-server.md) for the program that can reach
none of these and asks over HTTP instead.
