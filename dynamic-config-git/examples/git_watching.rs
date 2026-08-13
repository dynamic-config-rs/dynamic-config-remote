//! Configuration in a git repository, watched by polling the ref.
//!
//! ```text
//! # any local repository will do — this needs no network at all
//! mkdir -p /tmp/config-repo && cd /tmp/config-repo
//! git init --initial-branch=main
//! printf 'host: db.internal\nport: 5432\n' > db.yaml
//! git add db.yaml && git commit -m 'initial configuration'
//!
//! cargo run -p dynamic-config-git --example git_watching
//!
//! # ...then, in another shell, commit a change and watch it arrive
//! printf 'host: db-replica.internal\nport: 5432\n' > db.yaml
//! git commit -am 'move to the replica'
//! ```
//!
//! Point `GIT_URL` at `https://github.com/acme/config.git` and set
//! `GIT_TOKEN` to reach a private repository instead; nothing else changes.
//!
//! git cannot push, so this polls — but each tick is one ref advertisement
//! and no objects, and only a commit that actually landed costs a transfer.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_git::{Credential, GitSource};
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("GIT_URL").unwrap_or_else(|_| "file:///tmp/config-repo".to_owned());

    // A token pasted in is presented unchanged forever. Anything that expires
    // — a GitHub App installation token, an OIDC-exchanged token — belongs in
    // `Credential::expiring`, which is handed the credential it is replacing
    // and reports how long the new one lives.
    let credential = match std::env::var("GIT_TOKEN") {
        Ok(token) => Credential::token(token),
        Err(_) => Credential::anonymous(),
    };

    let source = GitSource::builder(&url)
        .branch("main")
        .path("db.yaml")
        .credential(credential)
        .build()?;

    println!("built a source for {}\n", source.describe());

    // Fetch first: a watch reports *changes*, so the starting value is loaded
    // explicitly rather than waited for.
    DbConfig::set_remote(source);
    DbConfig::refresh_remote()?;

    // No files: the fetched document is the whole configuration.
    DbConfig::builder("db").env("APP_").init()?;

    let started = DbConfig::current();
    println!("at start: {}:{}", started.host, started.port);

    // The sink is taken here, at wiring: it remembers which source is
    // installed, and a sink whose source is later replaced refuses to push.
    let sink = DbConfig::remote_sink();

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let watcher = std::thread::spawn(move || {
        let source = GitSource::builder(&url)
            .branch("main")
            .path("db.yaml")
            .build()
            .expect("the same source, for the watch loop's own working directory");

        source.watch(&watching, Duration::from_secs(5), move |document| {
            // The same reload path a file edit takes: validation, the reload
            // hooks, the diff, the cache.
            sink.apply(document)
        })
    });

    for _ in 0..24 {
        std::thread::sleep(Duration::from_secs(5));

        let now = DbConfig::current();
        println!("now: {}:{}", now.host, now.port);
    }

    // Dropping the handle would do the same; `stop` says it where somebody
    // decided it.
    watch.stop();
    let _ = watcher.join();

    Ok(())
}
