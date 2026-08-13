//! Vault behind a certificate authority this machine has never heard of.
//!
//! The enterprise case: an internal CA signs everything, the platform trust
//! store knows nothing about it, and the answer is to trust one more
//! certificate rather than to stop checking.
//!
//! ```text
//! # A dev-mode Vault, and a TLS-terminating proxy in front of it with a
//! # certificate signed by your own authority — `consul tls ca create`,
//! # `cfssl`, `step-ca`, or whatever your platform team already runs:
//! docker run --rm -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=myroot hashicorp/vault:1.17
//!
//! VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=myroot \
//!   vault kv put secret/myapp/db host=db.internal port=5432 password=hunter2
//!
//! VAULT_CACERT=/etc/ssl/private-ca.pem \
//!   cargo run -p dynamic-config-vault --example vault_private_ca
//! ```
//!
//! With no `VAULT_CACERT` set it runs against the plain dev server over HTTP,
//! so the example is runnable on a laptop with nothing installed. The
//! `with_tls(..)` line is the whole feature, and it is three lines whichever
//! store it is: the same [`TlsConfig`] goes to etcd, Consul, Redis, NATS, S3
//! and Firestore.

use dynamic_config::{dynamic_config, RemoteSource};
use dynamic_config_vault::{Auth, TlsConfig, Vault};
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

    let mut source = Vault::new(&address, "secret", "myapp/db").with_auth(Auth::token(
        std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "myroot".to_owned()),
    ));

    // The whole feature. `VAULT_CACERT` is the variable Vault's own CLI reads,
    // so a deployment that already sets it needs nothing new.
    //
    // Note what is *not* here: no `ureq` type, no TLS stack in the calling
    // code, nothing that would have to be spelled differently for a different
    // store. That is what makes this the surface a Python binding can reach.
    if let Ok(ca) = std::env::var("VAULT_CACERT") {
        println!("trusting the authority in {ca}");

        source = source.with_tls(TlsConfig::new().with_ca_certificate_file(&ca));
    } else {
        println!(
            "VAULT_CACERT is not set, so this runs against a plain dev server.\n\
             Point it at a PEM file to see the private-CA path."
        );
    }

    // Nothing has been read yet — not the secret, and not the CA file. A
    // missing certificate is an error from the first request that names the
    // path, rather than a panic in the builder chain above.
    println!("built a source for {}\n", source.describe());

    DbConfig::set_remote(source);
    DbConfig::refresh_remote()?;

    DbConfig::builder("db").env("APP_").init()?;

    let config = DbConfig::current();

    println!("host     = {}", config.host);
    println!("port     = {}", config.port);
    println!("password = {} (redacted by #[config(secret)])", {
        let _ = &config.password;
        "***"
    });

    // A CA *replaces* the platform trust store rather than joining it: naming
    // a private authority is saying the public ones do not apply to this host.
    // A deployment that needs both puts both certificates in the one file.
    //
    // And there is deliberately no way to turn verification off. The two
    // situations people reach for that in — a self-signed development server,
    // an enterprise private CA — are both this call.

    Ok(())
}
