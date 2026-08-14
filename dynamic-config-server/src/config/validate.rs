//! Which refusal applies, and the order the checks run in.
//!
//! Pure, and separate from starting, so the whole refusal surface is
//! testable without a socket: [`Server::start`](crate::Server::start) calls
//! [`validate`](ServerConfig::validate) first and does nothing else if it
//! says no.

use std::net::SocketAddr;

use super::{Refusal, ServerConfig};
use crate::auth::MIN_TOKEN_LEN;

impl ServerConfig {
    /// Every reason this configuration will not start a server.
    ///
    /// Pure, and separate from starting, so the whole refusal surface is
    /// testable without a socket. [`Server::start`](crate::Server::start)
    /// calls it first and does nothing else if it says no.
    ///
    /// # Errors
    ///
    /// The first [`Refusal`] that applies, in the order this checks them:
    /// the shape of the roster before the shape of the network, because an
    /// operator fixing two problems would rather be told about the one that
    /// is about who can read what.
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.sections.is_empty() {
            return Err(Refusal::NoSections);
        }
        if self.clients.is_empty() {
            return Err(Refusal::NoClients);
        }

        let mut seen = Vec::new();

        for section in &self.sections {
            // The predicate the handlers apply to the path segments, applied
            // where the mistake was made. Without this a section named with
            // a space in it — or one 65 characters long — loads, starts, and
            // reports ready, while every request for it is refused by
            // `is_name` before the section map is even consulted: a section
            // that exists and can never be reached.
            for (part, value) in [
                ("application", &section.application),
                ("profile", &section.profile),
            ] {
                if !crate::routes::is_name(value) {
                    return Err(Refusal::UnroutableSection {
                        application: section.application.clone(),
                        profile: section.profile.clone(),
                        part,
                    });
                }
            }

            let pair = (section.application.as_str(), section.profile.as_str());

            if seen.contains(&pair) {
                return Err(Refusal::DuplicateSection {
                    application: section.application.clone(),
                    profile: section.profile.clone(),
                });
            }

            seen.push(pair);
        }

        let mut names: Vec<&str> = Vec::new();
        let mut anonymous = 0;

        for client in &self.clients {
            if names.contains(&client.name.as_str()) {
                return Err(Refusal::DuplicateClient {
                    name: client.name.clone(),
                });
            }

            names.push(&client.name);

            match &client.token {
                None => {
                    anonymous += 1;

                    if !self.allow_anonymous {
                        return Err(Refusal::AnonymousNotAllowed {
                            client: client.name.clone(),
                        });
                    }
                    if anonymous > 1 {
                        return Err(Refusal::SeveralAnonymousClients);
                    }
                }
                Some(token) if token.len() < MIN_TOKEN_LEN => {
                    return Err(Refusal::WeakToken {
                        client: client.name.clone(),
                    });
                }
                Some(_) => {}
            }

            // A grant naming an application nothing serves is a typo that
            // reads as a working deployment right up to the first 404. Two
            // sections can share an application (one per profile), so the
            // grant is checked against the application names, not the pairs.
            for application in &client.applications {
                if !self
                    .sections
                    .iter()
                    .any(|section| &section.application == application)
                {
                    return Err(Refusal::UnservedGrant {
                        client: client.name.clone(),
                        application: application.clone(),
                    });
                }
            }
        }

        // Two clients sharing a token is not a smaller version of one client
        // with two grants: whichever is listed first silently wins, and the
        // audit log then names the wrong caller for every request.
        for (index, client) in self.clients.iter().enumerate() {
            let Some(token) = &client.token else { continue };

            for other in self.clients.iter().skip(index + 1) {
                if other.token.as_ref().is_some_and(|it| token.same_as(it)) {
                    return Err(Refusal::DuplicateToken);
                }
            }
        }

        let address = self
            .bind
            .parse::<SocketAddr>()
            .map_err(|_| Refusal::UnparsableBind {
                bind: self.bind.clone(),
            })?;

        // The whole matrix of TLS against the bind, in one place. Four
        // starting shapes and three refusals:
        //
        // | tls     | bind         | insecure | outcome                    |
        // |---------|--------------|----------|----------------------------|
        // | absent  | loopback     | either   | starts, in the clear       |
        // | absent  | non-loopback | false    | `ExposedBind`              |
        // | absent  | non-loopback | true     | starts; a terminator is in front |
        // | present | anything     | false    | starts, terminating TLS    |
        // | present | anything     | true     | `InsecureWithTls`          |
        // | present | anything     | —        | `TlsUnsupported` if the feature is off |
        match &self.tls {
            Some(tls) => {
                // A build without the feature has no rustls in it at all. The
                // block is still *parsed* — a key that only exists in some
                // builds would be an unknown field in the others, and this
                // crate refuses unknown fields — so the refusal is here,
                // where it can name the feature.
                if !cfg!(feature = "tls") {
                    return Err(Refusal::TlsUnsupported);
                }
                // Before the path checks, because this one is about who can
                // read what and they are about which file: an operator with
                // both problems would rather hear that revocation is not
                // checked than that a path is blank.
                if tls.crl.is_some() {
                    return Err(Refusal::RevocationUnsupported);
                }
                if tls.certificate.trim().is_empty() {
                    return Err(Refusal::TlsPathMissing { key: "certificate" });
                }
                if tls.key.trim().is_empty() {
                    return Err(Refusal::TlsPathMissing { key: "key" });
                }
                if tls
                    .client_ca
                    .as_ref()
                    .is_some_and(|it| it.trim().is_empty())
                {
                    return Err(Refusal::TlsPathMissing { key: "client_ca" });
                }
                if self.insecure {
                    return Err(Refusal::InsecureWithTls);
                }
            }
            None => {
                if !address.ip().is_loopback() && !self.insecure {
                    return Err(Refusal::ExposedBind {
                        bind: self.bind.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    /// The validated bind address.
    ///
    /// # Errors
    ///
    /// If `bind` is not a literal `address:port`. A hostname is refused
    /// rather than resolved: which of a name's addresses a server ends up
    /// on is not a thing to discover at startup.
    pub fn address(&self) -> Result<SocketAddr, Refusal> {
        self.bind
            .parse::<SocketAddr>()
            .map_err(|_| Refusal::UnparsableBind {
                bind: self.bind.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Token;
    use crate::config::{ClientConfig, SectionConfig, TlsConfig};

    fn section(application: &str, profile: &str) -> SectionConfig {
        SectionConfig {
            application: application.to_owned(),
            profile: profile.to_owned(),
            files: vec!["config.toml".to_owned()],
            env_prefix: None,
            whole_document: false,
        }
    }

    fn client(name: &str, token: Option<&str>, applications: &[&str]) -> ClientConfig {
        ClientConfig {
            name: name.to_owned(),
            token: token.map(Token::new),
            applications: applications.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    const GOOD: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn valid() -> ServerConfig {
        ServerConfig {
            sections: vec![section("billing", "prod")],
            clients: vec![client("billing-pod", Some(GOOD), &["billing"])],
            ..ServerConfig::default()
        }
    }

    #[test]
    fn a_complete_configuration_starts() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn an_empty_roster_is_refused_at_both_ends() {
        let mut config = valid();
        config.sections.clear();
        assert_eq!(config.validate(), Err(Refusal::NoSections));

        let mut config = valid();
        config.clients.clear();
        assert_eq!(config.validate(), Err(Refusal::NoClients));
    }

    #[test]
    fn a_duplicate_section_is_refused_but_two_profiles_are_not() {
        let mut config = valid();
        config.sections.push(section("billing", "prod"));

        assert_eq!(
            config.validate(),
            Err(Refusal::DuplicateSection {
                application: "billing".to_owned(),
                profile: "prod".to_owned(),
            })
        );

        let mut config = valid();
        config.sections.push(section("billing", "staging"));
        assert_eq!(config.validate(), Ok(()));
    }

    /// A section whose name no path segment can carry is refused where it
    /// was written, not answered `404` forever. The predicate is the
    /// handlers' own, so the two cannot drift.
    #[test]
    fn a_section_no_route_could_name_is_refused_at_startup() {
        for (part, application, profile) in [
            ("application", "billing api", "prod"),
            ("profile", "billing", ".hidden"),
            ("application", "", "prod"),
            ("profile", "billing", "../etc"),
        ] {
            let mut config = valid();
            config.sections = vec![section(application, profile)];
            config.clients = vec![client("pod", Some(GOOD), &[application])];

            assert_eq!(
                config.validate(),
                Err(Refusal::UnroutableSection {
                    application: application.to_owned(),
                    profile: profile.to_owned(),
                    part,
                }),
                "`{application}`/`{profile}` must be refused"
            );
        }

        // And the shapes a deployment actually uses still pass.
        let mut config = valid();
        config.sections = vec![section("billing-api.v2", "prod_1")];
        config.clients = vec![client("pod", Some(GOOD), &["billing-api.v2"])];
        assert_eq!(config.validate(), Ok(()));

        // Sixty-four characters is the ceiling, and it is inclusive.
        let mut config = valid();
        let long = "a".repeat(65);
        config.sections = vec![section(&long, "prod")];
        config.clients = vec![client("pod", Some(GOOD), &[&long])];
        assert!(matches!(
            config.validate(),
            Err(Refusal::UnroutableSection { .. })
        ));
    }

    #[test]
    fn duplicate_client_names_and_tokens_are_refused() {
        let mut config = valid();
        config
            .clients
            .push(client("billing-pod", Some(OTHER), &["billing"]));

        assert_eq!(
            config.validate(),
            Err(Refusal::DuplicateClient {
                name: "billing-pod".to_owned()
            })
        );

        let mut config = valid();
        config
            .clients
            .push(client("other", Some(GOOD), &["billing"]));

        assert_eq!(config.validate(), Err(Refusal::DuplicateToken));
    }

    #[test]
    fn a_short_token_is_refused() {
        let mut config = valid();
        config.clients = vec![client("billing-pod", Some("short"), &["billing"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::WeakToken {
                client: "billing-pod".to_owned()
            })
        );
    }

    /// The switch the threat model turns on: no credential is nobody unless
    /// the deployment says otherwise, in as many words.
    #[test]
    fn anonymous_access_needs_an_explicit_opt_in() {
        let mut config = valid();
        config.clients = vec![client("anonymous", None, &["billing"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::AnonymousNotAllowed {
                client: "anonymous".to_owned()
            })
        );

        config.allow_anonymous = true;
        assert_eq!(config.validate(), Ok(()));

        config.clients.push(client("also", None, &["billing"]));
        assert_eq!(config.validate(), Err(Refusal::SeveralAnonymousClients));
    }

    #[test]
    fn a_grant_nothing_serves_is_refused() {
        let mut config = valid();
        config.clients = vec![client("billing-pod", Some(GOOD), &["biling"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::UnservedGrant {
                client: "billing-pod".to_owned(),
                application: "biling".to_owned(),
            })
        );
    }

    #[test]
    fn a_non_loopback_bind_is_refused_without_the_flag() {
        let mut config = valid();
        config.bind = "0.0.0.0:8080".to_owned();

        let refusal = config.validate().unwrap_err();
        assert_eq!(
            refusal,
            Refusal::ExposedBind {
                bind: "0.0.0.0:8080".to_owned()
            }
        );
        assert!(
            refusal.to_string().contains("insecure"),
            "the refusal has to name the key that fixes it: {refusal}"
        );

        config.insecure = true;
        assert_eq!(config.validate(), Ok(()));
    }

    fn tls(client_ca: Option<&str>) -> TlsConfig {
        TlsConfig {
            certificate: "/etc/tls/server.pem".to_owned(),
            key: "/etc/tls/server.key".to_owned(),
            client_ca: client_ca.map(ToOwned::to_owned),
            crl: None,
        }
    }

    /// The half of the matrix that only exists because TLS does: terminating
    /// it here is itself the answer to "that address is not loopback", so no
    /// acknowledgement is asked for.
    #[cfg(feature = "tls")]
    #[test]
    fn tls_is_the_acknowledgement_a_non_loopback_bind_needs() {
        let mut config = valid();
        config.bind = "0.0.0.0:8443".to_owned();
        config.tls = Some(tls(None));

        assert_eq!(config.validate(), Ok(()));
    }

    /// And the refusal that keeps `insecure` meaning one thing. Without it,
    /// a configuration that had both would keep starting after the TLS block
    /// was deleted — in the clear, on a public address, having been
    /// pre-approved months earlier.
    #[cfg(feature = "tls")]
    #[test]
    fn insecure_and_tls_together_are_a_contradiction_rather_than_a_no_op() {
        let mut config = valid();
        config.bind = "0.0.0.0:8443".to_owned();
        config.tls = Some(tls(Some("/etc/tls/ca.pem")));
        config.insecure = true;

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::InsecureWithTls);
        assert!(refusal.to_string().contains("insecure"), "{refusal}");
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_tls_section_that_names_no_file_is_refused_per_key() {
        for (key, mut broken) in [
            ("certificate", tls(None)),
            ("key", tls(None)),
            ("client_ca", tls(Some(""))),
        ] {
            match key {
                "certificate" => broken.certificate = String::new(),
                "key" => broken.key = "   ".to_owned(),
                _ => {}
            }

            let mut config = valid();
            config.tls = Some(broken);

            assert_eq!(config.validate(), Err(Refusal::TlsPathMissing { key }));
        }
    }

    /// Revocation is refused rather than half-implemented, and the refusal
    /// has to name what to do instead — an operator who configured a CRL is
    /// an operator who has a certificate to withdraw, and leaving them with
    /// "no" and no answer is how the key ends up back in the file next week.
    #[cfg(feature = "tls")]
    #[test]
    fn a_crl_is_refused_and_the_refusal_names_the_credential_that_can_be_revoked() {
        let mut config = valid();
        let mut with_crl = tls(Some("/etc/tls/ca.pem"));
        with_crl.crl = Some("/etc/tls/clients.crl".to_owned());
        config.tls = Some(with_crl);

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::RevocationUnsupported);

        let rendered = refusal.to_string();

        assert!(rendered.contains("`tls.crl`"), "{rendered}");
        assert!(rendered.contains("token"), "{rendered}");
        assert!(rendered.contains("short-lived"), "{rendered}");
    }

    /// The key is *understood* rather than unknown, which is the whole point
    /// of it existing: `deny_unknown_fields` would otherwise answer an
    /// operator asking for revocation with "unknown field", which reads as a
    /// misspelling and sends them looking for the right one.
    #[test]
    fn a_crl_key_parses_so_that_the_refusal_can_explain_rather_than_serde() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}],
                "tls":{"certificate":"c.pem","key":"k.pem","crl":"clients.crl"}}"#,
        )
        .expect("the key is understood, not unknown");

        assert_eq!(
            config.tls.expect("the block parsed").crl.as_deref(),
            Some("clients.crl")
        );
    }

    /// A build with no TLS in it says so, rather than serving in the clear
    /// on a port the operator believes is encrypted. This is the one refusal
    /// that is about the binary rather than the file.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn a_build_without_the_feature_refuses_a_tls_section() {
        let mut config = valid();
        config.tls = Some(tls(None));

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::TlsUnsupported);
        assert!(refusal.to_string().contains("--features tls"), "{refusal}");
    }

    /// The block parses in *both* builds. It has to: `deny_unknown_fields`
    /// would otherwise turn a build without the feature into "unknown field
    /// `tls`", which reads as a typo rather than as a missing feature.
    #[test]
    fn a_tls_section_is_understood_whether_or_not_the_feature_is_on() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}],
                "tls":{"certificate":"c.pem","key":"k.pem","client_ca":"ca.pem"}}"#,
        )
        .expect("the shape is complete");

        let tls = config.tls.expect("the block is understood");

        assert_eq!(tls.certificate, "c.pem");
        assert_eq!(tls.client_ca.as_deref(), Some("ca.pem"));
    }

    /// Nothing above may have moved the plain-HTTP half of the matrix.
    #[test]
    fn without_tls_a_non_loopback_bind_still_needs_the_acknowledgement() {
        let mut config = valid();
        config.bind = "0.0.0.0:8080".to_owned();

        assert!(matches!(
            config.validate(),
            Err(Refusal::ExposedBind { .. })
        ));

        config.insecure = true;
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn ipv6_loopback_counts_as_loopback() {
        let mut config = valid();
        config.bind = "[::1]:8080".to_owned();

        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn a_hostname_is_refused_rather_than_resolved() {
        let mut config = valid();
        config.bind = "localhost:8080".to_owned();

        assert_eq!(
            config.validate(),
            Err(Refusal::UnparsableBind {
                bind: "localhost:8080".to_owned()
            })
        );
    }

    /// Refusals are printed at startup and end up in a log. None of them may
    /// carry a token there.
    #[test]
    fn no_refusal_prints_a_token() {
        let mut config = valid();
        config
            .clients
            .push(client("other", Some(GOOD), &["billing"]));

        let refusal = config.validate().unwrap_err();

        assert!(
            !refusal.to_string().contains(GOOD) && !format!("{refusal:?}").contains(GOOD),
            "a credential escaped through a refusal: {refusal}"
        );
    }

    /// The stream ceiling has a default a fleet does not reach, and zero is
    /// a legal value rather than a refusal: it is how a deployment says it
    /// does not want long-lived connections at all.
    #[test]
    fn the_stream_ceiling_defaults_high_and_zero_is_a_valid_answer() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}]}"#,
        )
        .expect("the shape is complete");

        assert_eq!(config.max_stream_connections, 4096);
        assert_eq!(config.validate(), Ok(()));

        let mut off = config;
        off.max_stream_connections = 0;
        assert_eq!(off.validate(), Ok(()));
    }

    #[test]
    fn a_key_the_server_does_not_know_is_refused() {
        let error = serde_json::from_str::<ServerConfig>(
            r#"{"sections":[],"clients":[],"allow_anonymou":true}"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"), "{error}");
    }
}
