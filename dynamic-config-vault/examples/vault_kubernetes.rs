//! Vault, authenticated the way a pod does, watched the way a secret store has
//! to be watched.
//!
//! ```text
//! # A dev-mode Vault, which mounts `secret/` as KV v2:
//! docker run --rm -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=myroot hashicorp/vault:1.17
//!
//! # Put a secret in it:
//! VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=myroot \
//!   vault kv put secret/myapp/db host=db.internal port=5432 password=hunter2
//!
//! cargo run -p dynamic-config-vault --example vault_kubernetes
//! ```
//!
//! It runs against the dev server with a root token, because that is what is
//! reachable from a laptop. The `Auth::kubernetes(..)` line is the one that
//! would change in a cluster — and it is commented in, not out, so the shape is
//! visible.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_vault::{Auth, Vault};
use serde::Deserialize;

#[dynamic_config]
#[derive(Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
    #[config(secret)]
    password: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address =
        std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8200".to_owned());

    // In a cluster this is the whole of it — no secret to distribute, and the
    // pod's own identity is the credential:
    //
    //     .with_auth(Auth::kubernetes("myapp"))
    //
    // On a laptop, a token from the dev server:
    let source = Vault::new(&address, "secret", "myapp/db").with_auth(Auth::token(
        std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "myroot".to_owned()),
    ));

    // Nothing has been reached yet: logging in is lazy, and so is fetching.
    println!("built a source for {}\n", source.describe());

    DbConfig::set_remote(source);

    DbConfig::refresh_remote()?;

    // No files: the fetched secret is the whole configuration. Initializing
    // through the builder is also what lets `apply_remote` reload later.
    let sources = DbConfig::builder("db").env("APP_");
    sources.init()?;

    let config = DbConfig::current();

    println!("host     = {}", config.host);
    println!("port     = {}", config.port);
    println!("password = {} (redacted by #[config(secret)])", {
        let _ = &config.password;
        "***"
    });
    println!("\ntraced back to: {:?}", DbConfig::source_of("host")?);

    // The environment still wins over the store, which is what makes a local
    // override possible without touching Vault.
    std::env::set_var("APP_DB_HOST", "localhost");
    sources.init()?;
    println!(
        "\nwith APP_DB_HOST set: host = {}",
        DbConfig::current().host
    );
    std::env::remove_var("APP_DB_HOST");
    // Reloaded, so the snapshot below is not still holding the override.
    sources.init()?;

    // ---------------------------------------------------------------------
    // Watching: metadata polling, so an unchanged secret is never read.
    // ---------------------------------------------------------------------
    println!("\nwatching for 5 seconds — try `vault kv put secret/myapp/db ...`");

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let source = Vault::new(&address, "secret", "myapp/db").with_auth(Auth::token(
        std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "myroot".to_owned()),
    ));

    let loop_handle = std::thread::spawn(move || {
        source.watch(&watching, Duration::from_secs(1), |document| {
            println!("  a new version arrived");

            DbConfig::apply_remote(document)
        })
    });

    std::thread::sleep(Duration::from_secs(5));

    // Dropping the handle would do the same; `stop` just says it out loud.
    watch.stop();
    loop_handle.join().expect("the loop should end, not hang")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
