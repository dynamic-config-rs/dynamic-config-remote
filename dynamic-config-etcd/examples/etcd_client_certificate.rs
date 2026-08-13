//! etcd with mutual TLS: a private authority to trust, and a client
//! certificate to present.
//!
//! An etcd cluster started with `--client-cert-auth` will not talk to a client
//! that cannot prove who it is. That is the ordinary hardened deployment
//! rather than an exotic one, and it is two lines here.
//!
//! ```text
//! # Generate an authority and two certificates — any of `cfssl`, `step-ca`
//! # or `openssl` will do; this is the shape, not a recipe:
//! #   ca.pem  server.pem/server-key.pem  client.pem/client-key.pem
//!
//! docker run --rm -p 2379:2379 \
//!   -v "$PWD/certs:/certs" quay.io/coreos/etcd:v3.5.17 \
//!   etcd --advertise-client-urls=https://0.0.0.0:2379 \
//!        --listen-client-urls=https://0.0.0.0:2379 \
//!        --cert-file=/certs/server.pem --key-file=/certs/server-key.pem \
//!        --trusted-ca-file=/certs/ca.pem --client-cert-auth
//!
//! ETCD_ENDPOINT=https://127.0.0.1:2379 \
//! ETCD_CA=certs/ca.pem \
//! ETCD_CLIENT_CERT=certs/client.pem \
//! ETCD_CLIENT_KEY=certs/client-key.pem \
//!   cargo run -p dynamic-config-etcd --example etcd_client_certificate --features tls
//! ```
//!
//! The three variables are all optional: with none of them set this runs
//! against a plaintext etcd on the default port, so the example is readable
//! without a certificate authority to hand.

use dynamic_config::{dynamic_config, AsyncRemoteSource};
use dynamic_config_etcd::{ConnectOptions, Etcd, TlsConfig};
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("ETCD_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:2379".to_owned());

    // `ConnectOptions` still carries everything that is not TLS — the user and
    // password, keep-alives, whatever `etcd-client` grows next. The escape
    // hatch did not go anywhere; the TLS half simply stopped requiring it.
    let options = ConnectOptions::new();

    let mut tls = TlsConfig::new();

    if let Ok(ca) = std::env::var("ETCD_CA") {
        println!("trusting the authority in {ca}");

        tls = tls.with_ca_certificate_file(ca);
    }

    // Both halves or neither: presenting a certificate with no key to prove it
    // is a misconfiguration rather than half of mTLS, so there is no way to
    // set one and forget the other.
    match (
        std::env::var("ETCD_CLIENT_CERT"),
        std::env::var("ETCD_CLIENT_KEY"),
    ) {
        (Ok(certificate), Ok(key)) => {
            println!("presenting the client certificate in {certificate}");

            tls = tls.with_client_certificate_files(certificate, key);
        }
        _ => println!(
            "ETCD_CLIENT_CERT and ETCD_CLIENT_KEY are not both set, so no \
             client certificate is presented."
        ),
    }

    // `TlsConfig` prints its shape and never its material: the private key is
    // withheld even here, where the whole point is to show what was
    // configured.
    println!("tls = {tls:?}\n");

    let source = if tls.is_empty() {
        Etcd::with_options([endpoint.as_str()], "myapp/db.json", options).await?
    } else {
        Etcd::with_tls([endpoint.as_str()], "myapp/db.json", options, &tls).await?
    };

    println!("built a source for {}\n", source.describe());

    DbConfig::set_remote_async(source);
    DbConfig::refresh_remote_async().await?;

    DbConfig::builder("db").init()?;

    let config = DbConfig::current();

    println!("host = {}", config.host);
    println!("port = {}", config.port);

    Ok(())
}
