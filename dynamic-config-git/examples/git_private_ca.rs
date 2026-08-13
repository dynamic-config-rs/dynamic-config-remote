//! A directory of configuration files, read from a git host behind a private
//! certificate authority.
//!
//! Two things at once, because in a real deployment they arrive together: an
//! enterprise GitLab whose certificate nothing on the machine trusts, and a
//! configuration split across several files that has to be read as **one**
//! document.
//!
//! ```text
//! # Against your own host — the case this example is written for.
//! export GIT_URL=https://gitlab.internal/acme/config.git
//! export GIT_CA_BUNDLE=/etc/ssl/certs/acme-root.pem
//! export GIT_TOKEN=glpat-...
//! # ...and, if the host wants a client certificate as well:
//! export GIT_CLIENT_CERT=/etc/ssl/app.crt GIT_CLIENT_KEY=/etc/ssl/app.key
//!
//! cargo run -p dynamic-config-git --example git_private_ca
//! ```
//!
//! With nothing set it runs against a local repository instead, so the
//! several-files half can be seen without a server anywhere:
//!
//! ```text
//! mkdir -p /tmp/config-repo/conf && cd /tmp/config-repo
//! git init --initial-branch=main
//! printf 'app:\n  db:\n    host: db.internal\n    port: 5432\n' > conf/db.yaml
//! printf 'app:\n  server:\n    port: 8080\n'                    > conf/server.yaml
//! git add conf && git commit -m 'initial configuration'
//!
//! cargo run -p dynamic-config-git --example git_private_ca
//! ```
//!
//! # What to copy
//!
//! The `tls` block and the `Keys::prefix` line. Everything else is the
//! ordinary `dynamic-config` wiring this crate's other example already shows.
//!
//! Two rules worth carrying with them:
//!
//! - **`tls` is the `https://` knob and only that one.** An `ssh://` remote
//!   authenticates its host through `known_hosts` and its client through a key,
//!   which is `Credential::ssh_agent`, `ssh_key` or `ssh_command`. Asking for
//!   both is refused at `build()` rather than half-applied — which is why the
//!   call below is behind a check on the scheme.
//! - **There is no way to turn verification off**, and there will not be. A
//!   fetch presents its credential before it has received anything, so an
//!   unverified connection is one that hands a token to whoever is on the path.
//!   Trusting one more certificate is what `with_ca_certificate_file` is, and
//!   it keeps the server authenticated.

use std::time::Duration;

use dynamic_config::{dynamic_config, Format, RemoteSource};
use dynamic_config_git::{Credential, GitSource, Keys, TlsConfig};
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct AppConfig {
    db: Db,
    server: Server,
}

#[derive(Debug, Deserialize)]
struct Db {
    host: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
struct Server {
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("GIT_URL").unwrap_or_else(|_| "file:///tmp/config-repo".to_owned());

    let credential = match std::env::var("GIT_TOKEN") {
        // A token pasted in is presented unchanged forever. Anything that
        // expires — a GitLab CI job token, a GitHub App installation token —
        // belongs in `Credential::expiring`, which is handed the credential it
        // is replacing and reports how long the new one lives.
        Ok(token) => Credential::token(token),
        Err(_) => Credential::anonymous(),
    };

    let mut source = GitSource::builder(&url)
        .branch("main")
        // **A directory, not a string prefix.** Every file under `conf/` is
        // folded into one document as disjoint sections: `conf/db.yaml`
        // supplies `app.db`, `conf/server.yaml` supplies `app.server`, and two
        // files supplying the same path is a deployment bug reported by name
        // rather than resolved by whichever the tree happened to list first.
        //
        // For files where one is *meant* to win — a base and an override —
        // name them instead: `Keys::several(["conf/base.yaml",
        // "conf/local.yaml"])` merges in that order and the later one wins.
        .path(Keys::prefix("conf"))
        // A directory has no extension to read a format from, so it is stated.
        .format(Format::Yaml)
        .credential(credential)
        .with_timeout(Duration::from_secs(30));

    // Only for an https remote: `tls` configures that transport and nothing
    // else, and this crate refuses it on any other rather than pretending.
    if url.starts_with("https://") {
        if let Ok(bundle) = std::env::var("GIT_CA_BUNDLE") {
            let mut tls = TlsConfig::new()
                // The file may hold a chain — a private root and its
                // intermediate — and all of it is trusted. The platform's own
                // store still applies too, so this same source configuration
                // also reaches a public host.
                .with_ca_certificate_file(bundle);

            // mTLS, for a host that wants to know who is asking before it
            // answers. The private key is read when the client is built, never
            // rendered, and never quoted into an error.
            if let (Ok(certificate), Ok(key)) = (
                std::env::var("GIT_CLIENT_CERT"),
                std::env::var("GIT_CLIENT_KEY"),
            ) {
                tls = tls.with_client_certificate_files(certificate, key);
            }

            source = source.tls(tls);
        }
    }

    let source = source.build()?;

    // Before anything is read this names the ref; after a fetch it names the
    // **commit**, which is the first question of every configuration-in-git
    // incident — not "which branch" but "which commit is this process actually
    // serving". It is what reaches `explain` and every diagnostic below.
    println!("reading {}", source.describe());

    // Fetching is explicit, and the load that follows touches no network.
    AppConfig::set_remote(source);
    AppConfig::refresh_remote()?;

    // No files: the folded document is the whole configuration.
    AppConfig::builder("app").env("APP_").init()?;

    let current = AppConfig::current();

    println!(
        "database {}:{}, server on {}",
        current.db.host, current.db.port, current.server.port
    );

    Ok(())
}
