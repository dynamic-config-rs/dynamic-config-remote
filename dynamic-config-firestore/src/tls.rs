//! Translating the shared [`TlsConfig`] into `ureq`'s own TLS types.
//!
//! Byte for byte the same file in `dynamic-config-consul`,
//! `dynamic-config-firestore` and `dynamic-config-vault`, and deliberately
//! **not** in `dynamic-config-store-core`: putting it there would mean putting
//! `ureq` there, and `store-core` sits under etcd, NATS, Redis and S3 as well
//! — none of which have an HTTP client in their tree, and none of which
//! should grow one because three siblings share a PEM parser. The shared
//! *vocabulary* is in `store-core`; only the translation is copied.
//!
//! The claim in the first sentence is checked rather than asked for:
//! `dynamic-config-store-core/tests/copies.rs` compares the three files and
//! fails if one has moved without the others.

use std::time::Duration;

use dynamic_config::Error;
use dynamic_config_store_core::tls::TlsConfig;
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts};

/// An agent that trusts `tls`'s certificate authority and presents its client
/// certificate.
///
/// # Errors
///
/// If a file cannot be read, or if what was read is not PEM.
pub(crate) fn agent(
    tls: &TlsConfig,
    timeout: Duration,
    described: &str,
) -> Result<ureq::Agent, Error> {
    let mut config = ureq::tls::TlsConfig::builder();

    if let Some(pem) = tls.ca_certificate_pem(described)? {
        // `RootCerts::Specific` *replaces* the trust store rather than adding
        // to it, which is what pinning a private CA means: a deployment that
        // names its own authority is saying the public ones do not apply to
        // this host. A caller who wants both puts both in the file.
        let roots = certificates(&pem, described, "the CA certificate")?;
        config = config.root_certs(RootCerts::new_with_certs(&roots));
    }

    if let Some((certificate, key)) = tls.client_certificate_pem(described)? {
        let chain = certificates(&certificate, described, "the client certificate")?;

        // The upstream error is deliberately dropped rather than wrapped.
        // `rustls-pki-types` renders the line it choked on, and the line it
        // choked on in a private key file *is* private key material. What
        // went wrong is "this is not a PEM private key", and that is the
        // whole of what is safe to say.
        let key = PrivateKey::from_pem(&key)
            .map_err(|_| malformed(described, "the client private key"))?;

        config = config.client_cert(Some(ClientCert::new_with_certs(&chain, key)));
    }

    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(config.build())
        .build()
        .new_agent())
}

/// Every certificate in a PEM bundle — not only the first, which is what a
/// private CA with an intermediate needs.
fn certificates(
    pem: &[u8],
    described: &str,
    what: &str,
) -> Result<Vec<Certificate<'static>>, Error> {
    let mut certificates = Vec::new();

    for item in ureq::tls::parse_pem(pem) {
        // Dropped for the same reason as the key's: a bundle and a key
        // routinely share a file, so the parser's message is not safe to
        // quote even when the material is meant to be public.
        match item.map_err(|_| malformed(described, what))? {
            PemItem::Certificate(certificate) => certificates.push(certificate),
            // A key in a certificate bundle is a packaging habit, not an
            // error; it is simply not a certificate.
            _ => continue,
        }
    }

    if certificates.is_empty() {
        return Err(malformed(described, what));
    }

    Ok(certificates)
}

/// The one wording for material that is not what it claims to be.
///
/// Names what failed and nothing about what was in it.
fn malformed(described: &str, what: &str) -> Error {
    Error::remote(format!(
        "{described}: {what} is not PEM-encoded material of the kind expected"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate authority and a client certificate signed by it,
    /// generated here. A committed fixture expires, and a suite that fails on
    /// a date nobody chose is worse than one that costs a millisecond.
    fn material() -> (String, String, String) {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
        let client_key = KeyPair::generate().unwrap();
        let client = CertificateParams::new(vec!["myapp".to_owned()])
            .unwrap()
            .signed_by(&client_key, &issuer)
            .unwrap();

        (ca.pem(), client.pem(), client_key.serialize_pem())
    }

    /// The translation itself: what the caller said, arriving in `ureq`'s own
    /// configuration. Asserted on the agent rather than through a handshake
    /// because it pins the *mapping* — that a CA replaces the trust store
    /// rather than being added beside it, and that both halves of the client
    /// certificate travel together.
    #[test]
    fn the_shared_vocabulary_reaches_ureqs_own_configuration() {
        let (ca, certificate, key) = material();

        let agent = agent(
            &TlsConfig::new()
                .with_ca_certificate_pem(ca)
                .with_client_certificate_pem(certificate, key),
            Duration::from_secs(3),
            "store the-key",
        )
        .expect("the generated material is valid PEM");

        let tls = agent.config().tls_config();

        match tls.root_certs() {
            RootCerts::Specific(certificates) => assert_eq!(
                certificates.len(),
                1,
                "the named authority replaces the trust store rather than \
                 joining it"
            ),
            other => panic!("the CA did not reach the agent: {other:?}"),
        }

        assert_eq!(
            tls.client_cert()
                .expect("the client certificate reached the agent")
                .certs()
                .len(),
            1
        );

        assert!(
            !tls.disable_verification(),
            "there is no spelling for this, and nothing may turn it on by \
             accident"
        );
    }

    /// A bundle is a CA and its intermediates, and trusting only the first
    /// certificate in the file is a deployment that works until the day the
    /// intermediate is the one presented.
    #[test]
    fn every_certificate_in_a_bundle_is_trusted_rather_than_only_the_first() {
        let (ca, certificate, _) = material();

        let agent = agent(
            &TlsConfig::new().with_ca_certificate_pem(format!("{ca}{certificate}")),
            Duration::from_secs(3),
            "store the-key",
        )
        .expect("a bundle is valid PEM");

        match agent.config().tls_config().root_certs() {
            RootCerts::Specific(certificates) => assert_eq!(certificates.len(), 2),
            other => panic!("the bundle did not reach the agent: {other:?}"),
        }
    }

    /// The sharpest rule here: a key that will not parse must not put itself
    /// into the message explaining that it will not parse. The parser
    /// underneath renders the line it choked on, which is why its error is
    /// dropped rather than wrapped.
    #[test]
    fn a_malformed_private_key_never_quotes_itself_into_the_error() {
        const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

        let (ca, certificate, _) = material();

        let error = agent(
            &TlsConfig::new()
                .with_ca_certificate_pem(ca)
                .with_client_certificate_pem(
                    certificate,
                    format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
                ),
            Duration::from_secs(3),
            "store the-key",
        )
        .expect_err("the key is not a key");

        let printed = error.to_string();

        assert!(!printed.contains(PLANTED), "{printed}");
        assert!(printed.contains("the client private key"), "{printed}");
    }

    /// A CA that is not a certificate is refused rather than quietly leaving
    /// the platform trust store in place — which would be a program believing
    /// it is pinned and is not.
    #[test]
    fn material_that_holds_no_certificate_is_refused_rather_than_ignored() {
        let error = agent(
            &TlsConfig::new().with_ca_certificate_pem("not a certificate at all"),
            Duration::from_secs(3),
            "store the-key",
        )
        .expect_err("there is no certificate in there");

        assert!(error.to_string().contains("the CA certificate"), "{error}");
    }

    /// A missing file is reported here, with its path, rather than as a panic
    /// somewhere in a builder chain.
    #[test]
    fn a_missing_file_names_the_path_and_the_material() {
        let error = agent(
            &TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"),
            Duration::from_secs(3),
            "store the-key",
        )
        .expect_err("the file is not there");

        assert!(
            error.to_string().contains("/nonexistent/private-ca.pem"),
            "{error}"
        );
    }
}
