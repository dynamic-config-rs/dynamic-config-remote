//! The client half of the compose pair: a `ConfigServer` source feeding
//! an ordinary configuration, polled so an edit on the server reaches
//! this process without a restart.
//!
//! ```text
//! cd examples/compose && docker compose up -d
//! cargo run -p dynamic-config-server --example served --features client
//! ```

use std::time::Duration;

use dynamic_config::dynamic_config;
use dynamic_config_server::client::ConfigServer;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct Billing {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = ConfigServer::new("http://localhost:8155", "billing", "prod")
        .with_token("dev-token-not-for-production-32ch");

    Billing::set_remote(source);
    Billing::refresh_remote()?;

    // No files: the served document is the whole configuration.
    Billing::builder("billing").init()?;

    println!("serving from the config server; edit the document to move it");

    for _ in 0..6 {
        let billing = Billing::current();
        println!("  {}:{}", billing.host, billing.port);

        std::thread::sleep(Duration::from_secs(5));
        Billing::refresh_remote()?;
        Billing::builder("billing").reload()?;
    }

    Ok(())
}
