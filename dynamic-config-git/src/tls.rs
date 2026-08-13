//! HTTPS to a host this machine does not already trust.
//!
//! An enterprise GitLab behind a private certificate authority, or a host that
//! wants a client certificate before it will say hello, is an ordinary
//! deployment and this crate could not reach either one. The vocabulary is
//! [`TlsConfig`], which is `dynamic-config-store-core`'s and shared with every
//! other store crate; what is here is the part that is git's alone, which is
//! **where that configuration has to be put**.
//!
//! # What `gix` allows, measured
//!
//! `gix` is configured here with `blocking-http-transport-reqwest-rust-tls` —
//! pure Rust, no C toolchain and no OpenSSL question, which is why this
//! workspace has no C dependency. Its HTTP options type,
//! [`gix::protocol::transport::client::blocking_io::http::Options`], *has* the
//! two fields this needs — `ssl_ca_info` and `ssl_verify` — and `gix` even maps
//! `http.sslCAInfo` and `GIT_SSL_CAINFO` onto them. **Only the `curl` backend
//! reads them.** The `reqwest` backend clones that options struct and uses
//! three of its fields — the extra headers, the redirect policy and its own
//! backend hook — and never looks at either of the SSL ones. Its
//! `reqwest::blocking::Client` is built once, inside a worker thread, from a
//! `ClientBuilder` with no root store, no identity and no hook to reach either.
//! Its one extension point, `configure_request`, configures a *request*: a
//! request has a URL, a method, headers and a body, and no TLS anywhere in it.
//!
//! So setting `http.sslCAInfo` through this crate would do nothing at all, and
//! a store that silently ignores "trust this certificate authority" is a
//! program that believes it is pinned to a private CA and is not. The C
//! transport would read it, and adding a C TLS stack to a workspace that has
//! none — for a crate whose whole argument is that it needs no toolchain — is
//! the wrong trade.
//!
//! # What is here instead
//!
//! `gix` takes a transport of the caller's own — that is what
//! [`Remote::to_connection_with_transport`] is for — and its HTTP transport is
//! generic over a small [`Http`] trait: three methods over `GET`, `POST` and a
//! backend hook. So the *git* half stays `gix`'s entirely, including the
//! handshake, the protocol version, the credential header and the packet
//! framing; only the seven lines that build an HTTP client are ours, and those
//! are the seven lines a root store and an identity go into.
//!
//! It is used **only when a [`TlsConfig`] was configured**. A source that asks
//! for nothing new keeps `gix`'s own transport, byte for byte, and nothing on
//! this page can have broken it.
//!
//! # Which knob applies to which transport
//!
//! | Reaching | Configured with | Not with |
//! |---|---|---|
//! | `https://` | [`Builder::tls`](crate::Builder::tls) — a CA, a client certificate | any `Credential::ssh_*` |
//! | `ssh://`, `git@host:repo` | [`Credential::ssh_agent`](crate::Credential::ssh_agent), [`ssh_key`](crate::Credential::ssh_key), [`ssh_command`](crate::Credential::ssh_command) | `tls` — `ssh` has its own trust model, in `known_hosts` |
//! | `file://`, a path | nothing | either |
//!
//! They are refused rather than ignored: a `tls` on an `ssh://` url fails at
//! [`Builder::build`](crate::Builder::build), where the mistake was made.
//!
//! # The deadline this transport keeps, and the one `gix`'s cannot
//!
//! [`with_timeout`](crate::Builder::with_timeout) is a bound on one fetch, and
//! most of a fetch is bounded by `gix`'s interrupt flag, which it checks
//! between packets while negotiating and while receiving a pack. What an
//! interrupt flag cannot bound is a host that accepts the connection and then
//! sends nothing: there are no packets for the check to be between. On this
//! transport the client is ours, so the caller's number is the connect deadline
//! *and* the stall deadline on every read. On `gix`'s it is neither.
//!
//! Closing that on `gix`'s transport is an upstream change, and a small one:
//! `gix_transport`'s HTTP options already carry a `connect_timeout`, populated
//! from `gitoxide.http.connectTimeout`, and its `reqwest` backend never reads
//! it — the same way it never reads `ssl_ca_info`. Only `curl` does. Measured
//! against `gix-transport` 0.58.1, where the backend hardcodes twenty seconds.
//!
//! Two ways to close it from *here* were measured and both were refused:
//!
//! - **Through the backend hook.** `gix`'s reqwest backend takes a
//!   `configure_request` closure, and `reqwest::blocking::Request` has a
//!   `timeout_mut`, so a deadline could be installed without touching `gix` at
//!   all. Installing the hook is also what makes that backend treat the request
//!   as one whose headers must not be replayed, which turns its redirect policy
//!   into `RejectConfiguredHeaders` — every source would silently stop
//!   following the redirect `git` itself follows, in exchange for a timeout.
//! - **Using this transport for every `https://` source.** What `gix`'s
//!   reqwest backend reads out of the options it is handed is three fields —
//!   the extra headers, the redirect policy and this hook — so the swap would
//!   cost `http.extraHeader` and redirect following and nothing else, which is
//!   less than it sounds. It is still the wrong trade: it puts every caller,
//!   including every caller whose host answers in milliseconds and who never
//!   asked for TLS, on this crate's HTTP client to bound a stall that `gix` is
//!   one field away from bounding itself.
//!
//! So the honest thing is to say which transport bounds what, which
//! [`with_timeout`](crate::Builder::with_timeout) does in a table.
//!
//! # It follows no redirect, and that is not the obvious reason
//!
//! The obvious reason is that every request here carries an `Authorization`
//! header and a redirect is a stranger's opportunity to be handed it — and it
//! is not quite the true one: `reqwest` removes `Authorization` itself when a
//! redirect crosses to another host, port or scheme, so the token would not
//! travel. The real reason is what a git fetch *is*.
//!
//! Smart HTTP is two requests against one base url: a `GET` of the ref
//! advertisement and a `POST` of the negotiation. Following a `301` on the
//! first leaves the second still addressed to the old url — and a `301` on a
//! `POST` is turned into a `GET` with no body by every HTTP client that obeys
//! the specification, this one included. Making a redirect work therefore means
//! rewriting the base url for the rest of the conversation, verifying that what
//! changed was only the part that may change, and deciding whether the identity
//! may be reused at the new address. That is `gix`'s `redirect` module and the
//! bookkeeping around it: security-relevant code, and not worth a second copy
//! for a transport that is only reached by callers who named an unusual host.
//! Such a host is named by its final url instead, and the error says so.
//!
//! # There is no way to turn verification off
//!
//! The reasoning is [`dynamic_config_store_core::tls`]'s and it holds here
//! unchanged: the two situations anybody reaches for it in — a development
//! server with a self-signed certificate, an enterprise private CA — are both
//! *trusting one more certificate*, which is
//! [`with_ca_certificate_file`](TlsConfig::with_ca_certificate_file) and keeps
//! the server authenticated. Turning verification off does not make TLS weaker
//! the way a checklist means; it makes it absent.
//!
//! git gives that a second, sharper edge. A fetch presents a credential — the
//! `Authorization` header this crate puts on every request — before it has
//! received anything. A connection with no verification is one any party on the
//! path can terminate, and what they get for it is the token. So the knob a
//! caller would reach for "just to get past a certificate error in staging" is
//! the knob that hands a personal access token to whoever is in the way, and
//! there is no name frightening enough to fix that. `gix`'s own
//! `gitoxide.http.sslNoVerify` is not reachable from here either, because the
//! transport on this page is the one being used and it never reads it.
//!
//! [`Remote::to_connection_with_transport`]: gix::Remote::to_connection_with_transport

use std::io::{BufRead, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dynamic_config::Error;
use dynamic_config_store_core::tls::TlsConfig;
use gix::protocol::transport::client::blocking_io::http::{
    Error as HttpError, GetResponse, Http, PostBodyDataKind, PostResponse,
};

use crate::url::redacted;

/// The transport `gix` drives, once a [`TlsConfig`] means it cannot use its
/// own.
pub(crate) type Transport = gix::protocol::transport::client::blocking_io::http::Transport<Client>;

/// Refuses a [`TlsConfig`] on a url that has no TLS in it.
///
/// A `tls` on `ssh://` is a caller who believes a certificate authority is what
/// authenticates an SSH host; it is not — `known_hosts` is — and quietly doing
/// nothing would leave them believing it. Checked at `build`, where it was
/// written.
///
/// # Errors
///
/// If `tls` asks for anything and `url` is not `https://`.
pub(crate) fn check_scheme(url: &str, tls: &TlsConfig) -> Result<(), Error> {
    if tls.is_empty() || url.starts_with("https://") {
        return Ok(());
    }

    Err(Error::remote(format!(
        "git {}: `tls` configures the https transport, and this url is not an \
         https one; an ssh remote authenticates its host through `known_hosts` \
         and its client through a key, which is `Credential::ssh_agent`, \
         `ssh_key` or `ssh_command`",
        redacted(url)
    )))
}

/// The transport for one fetch, with the caller's trust material in it.
///
/// Built per fetch rather than kept, for the same reason the repository is:
/// a rotated CA file or a re-issued client certificate is picked up by the next
/// fetch instead of by a restart.
///
/// # Errors
///
/// If a PEM file cannot be read, if what it holds is not a certificate or a
/// private key, or if a TLS client cannot be built from them. No message
/// carries any of the material — see [`described`](Self).
pub(crate) fn transport(
    url: &gix::Url,
    version: gix::protocol::transport::Protocol,
    tls: &TlsConfig,
    timeout: Duration,
    trace: bool,
    described: &str,
) -> Result<Transport, Error> {
    Ok(Transport::new_http(
        Client::new(tls, timeout, described)?,
        url.clone(),
        version,
        trace,
    ))
}

/// A `reqwest` client, wearing the one trait `gix`'s HTTP transport needs.
pub(crate) struct Client {
    client: reqwest::blocking::Client,
    /// The source's `describe()`, for the errors this half raises itself.
    described: String,
    /// The caller's [`with_timeout`](crate::Builder::with_timeout), kept so a
    /// request that hit it can say which number it hit.
    timeout: Duration,
}

// Hand-written for the reason every `Debug` in this crate is. `reqwest`'s own
// does not print an identity today, and this type is not the place to depend on
// that staying true: the client here may be holding a private key.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("for", &self.described)
            .finish_non_exhaustive()
    }
}

impl Client {
    fn new(tls: &TlsConfig, timeout: Duration, described: &str) -> Result<Self, Error> {
        let mut builder = reqwest::blocking::ClientBuilder::new()
            // `gix`'s own reqwest transport hardcodes twenty seconds here and
            // exposes no way to change it, which is the one part of
            // `with_timeout` this crate has always had to document as not
            // covered. On this path it is covered.
            .connect_timeout(timeout)
            // And so is the part a connect deadline does not reach. `reqwest`
            // applies this to the connect, to the response head, and to each
            // read of the body — a stall bound rather than a budget for the
            // whole transfer, so a large pack over a slow link is not cut off
            // while a host that accepts the connection and then says nothing is
            // given up on. That case is otherwise unbounded: `gix`'s interrupt
            // flag is checked between packets, and a host that sends none never
            // reaches a between.
            .timeout(timeout)
            .http1_title_case_headers()
            // **No redirects, deliberately.** Not because the credential would
            // travel — `reqwest` strips `Authorization` across a change of
            // host, port or scheme — but because a fetch is two requests
            // against one base url, and following a redirect on the first
            // leaves the second addressed to the old one. See the module
            // documentation for what making it work would actually cost.
            .redirect(reqwest::redirect::Policy::none());

        if let Some(pem) = tls.ca_certificate_pem(described)? {
            // A bundle rather than one certificate: a private CA with an
            // intermediate is the ordinary enterprise shape, and a file
            // holding both must trust both.
            let authorities = reqwest::Certificate::from_pem_bundle(&pem).map_err(|error| {
                // `error` is `reqwest`'s own rendering of a parse failure. It
                // names the position and the reason, never the bytes — which
                // matters less for a certificate than for the key below, and
                // is checked by a test for both.
                Error::remote(format!(
                    "{described}: the CA certificate is not a PEM certificate: {error}"
                ))
            })?;

            // Measured, and the reason this check exists: a bundle of bytes
            // that are not PEM at all parses as **no** certificates rather than
            // as an error. Adding none of them would leave a program that asked
            // to trust a private CA trusting only the platform's roots and
            // never being told — the exact silent success this whole module is
            // an argument against.
            if authorities.is_empty() {
                return Err(Error::remote(format!(
                    "{described}: the CA certificate is not a PEM certificate: it holds \
                     no `BEGIN CERTIFICATE` block at all"
                )));
            }

            for certificate in authorities {
                // Additive: the platform's own trust store still applies, so a
                // deployment that reaches both its private GitLab and
                // github.com needs one source configuration rather than two.
                builder = builder.add_root_certificate(certificate);
            }
        }

        if let Some((certificate, key)) = tls.client_certificate_pem(described)? {
            let mut pem = certificate;
            pem.extend_from_slice(b"\n");
            pem.extend_from_slice(&key);

            let identity = reqwest::Identity::from_pem(&pem).map_err(|error| {
                // The only error path in this crate that has a private key in
                // scope. `reqwest`'s rendering of a bad key does not quote it,
                // and `a_bad_client_key_is_refused_without_quoting_it` holds
                // that rather than trusting it.
                Error::remote(format!(
                    "{described}: the client certificate and key are not a \
                     usable PEM pair: {error}"
                ))
            })?;

            builder = builder.identity(identity);
        }

        let client = builder.build().map_err(|error| {
            Error::remote(format!("{described}: cannot build a TLS client: {error}"))
        })?;

        Ok(Self {
            client,
            described: described.to_owned(),
            timeout,
        })
    }

    /// One request, sent when the first of its two readers is read.
    ///
    /// Deferred because the trait's `post` hands the caller a writer for the
    /// request body and the response readers *at the same time*: the body is
    /// not complete until the writer is dropped, so the request cannot go out
    /// in `post` itself.
    fn exchange(
        &self,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: Option<PostBodyDataKind>,
    ) -> Arc<Mutex<Exchange>> {
        let mut header_map = reqwest::header::HeaderMap::new();

        for line in headers {
            // A malformed header line is skipped rather than fatal, which is
            // what `gix`'s own backend does: `http.extraHeader` is arbitrary
            // caller configuration and a bad one should not stop a fetch.
            let Some((name, value)) = line.as_ref().split_once(':') else {
                continue;
            };

            if let Ok(name) = reqwest::header::HeaderName::try_from(name) {
                if let Ok(value) = reqwest::header::HeaderValue::try_from(value.trim()) {
                    header_map.append(name, value);
                }
            }
        }

        Arc::new(Mutex::new(Exchange::Pending {
            client: self.client.clone(),
            described: self.described.clone(),
            timeout: self.timeout,
            url: url.to_owned(),
            headers: header_map,
            posting: body.is_some(),
            body: Vec::new(),
        }))
    }
}

/// One request's three halves, and the one place they meet.
///
/// `gix` is handed a writer for the request body and two readers for the
/// response, and reads the headers to the end before touching the body. So the
/// request goes out when the first reader asks for something, and whichever
/// reader that was leaves the rest here for the other.
enum Exchange {
    Pending {
        client: reqwest::blocking::Client,
        described: String,
        timeout: Duration,
        url: String,
        headers: reqwest::header::HeaderMap,
        posting: bool,
        body: Vec<u8>,
    },
    /// The request went out and the host answered; `headers` is the rendered
    /// header block and `response` is the body, unread.
    Answered {
        headers: Vec<u8>,
        response: Option<reqwest::blocking::Response>,
    },
    /// It failed, and both readers owe the caller the same reason.
    Failed(String, std::io::ErrorKind),
}

impl Exchange {
    /// Sends the request if it has not gone out yet.
    fn send(&mut self) {
        let Self::Pending {
            client,
            described,
            timeout,
            url,
            headers,
            posting,
            body,
        } = self
        else {
            return;
        };

        let request = if *posting {
            client.post(url.as_str()).body(std::mem::take(body))
        } else {
            client.get(url.as_str())
        };

        let described = described.clone();
        let timeout = *timeout;

        *self = match request.headers(std::mem::take(headers)).send() {
            // The deadline, named as itself. `reqwest` renders this as "error
            // sending request … operation timed out", which reads as an outage
            // rather than as the number the caller chose — and the difference
            // decides whether the answer is to raise `with_timeout` or to go
            // and look at the host.
            Err(error) if error.is_timeout() => Self::Failed(
                format!(
                    "{described}: the host did not answer within {timeout:?}; \
                     raise `with_timeout` if that is too short for this repository"
                ),
                std::io::ErrorKind::TimedOut,
            ),

            Err(error) => Self::Failed(
                // The **chain**, not the top: `reqwest`'s own `Display` for a
                // failed request is "error sending request for url (…)", and
                // the reason — "invalid peer certificate: UnknownIssuer" — is
                // two `source()` calls below it. That reason is the whole
                // diagnostic value of this feature.
                //
                // The url it carries is `gix`'s, built from the remote's own,
                // so it goes through the same redaction every other message in
                // this crate does.
                format!("{described}: {}", redacted(&crate::fetch::chain(&error))),
                std::io::ErrorKind::Other,
            ),

            Ok(response) => {
                let status = response.status();

                if status.is_success() {
                    let mut rendered = Vec::new();

                    // `name:value\n` per line, which is what `gix` splits on a
                    // colon and compares case-insensitively.
                    for (name, value) in response.headers() {
                        rendered.extend_from_slice(name.as_str().as_bytes());
                        rendered.push(b':');
                        rendered.extend_from_slice(value.as_bytes());
                        rendered.push(b'\n');
                    }

                    Self::Answered {
                        headers: rendered,
                        response: Some(response),
                    }
                } else if status.is_redirection() {
                    Self::Failed(
                        format!(
                            "{described}: the host answered {status} — a redirect, which \
                             this transport does not follow, because a fetch is two \
                             requests against one base url and only this host knows the \
                             new one; name the url it is redirecting to",
                        ),
                        std::io::ErrorKind::Other,
                    )
                } else {
                    // The classification `gix`'s own backends promise, and the
                    // one this crate's `Failure::Refused` is built on: 401 is a
                    // credential that can be replaced, everything else is not.
                    Self::Failed(
                        format!("{described}: the host answered HTTP {status}"),
                        if status == reqwest::StatusCode::UNAUTHORIZED {
                            std::io::ErrorKind::PermissionDenied
                        } else if status.is_server_error() {
                            std::io::ErrorKind::ConnectionAborted
                        } else {
                            std::io::ErrorKind::Other
                        },
                    )
                }
            }
        };
    }
}

/// Which half of an [`Exchange`] a [`Reader`] is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Half {
    Headers,
    Body,
}

/// One half of a response, which sends the request if nobody has yet.
pub(crate) struct Reader {
    exchange: Arc<Mutex<Exchange>>,
    half: Half,
    /// Taken out of the exchange on the first read. Headers are small enough to
    /// hold; a body is the pack, and streams.
    inner: Option<Box<dyn BufRead>>,
}

impl Reader {
    fn ready(&mut self) -> std::io::Result<&mut Box<dyn BufRead>> {
        if self.inner.is_none() {
            let mut exchange = self
                .exchange
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            exchange.send();

            self.inner = Some(match &mut *exchange {
                Exchange::Failed(why, kind) => {
                    return Err(std::io::Error::new(*kind, why.clone()));
                }

                Exchange::Answered { headers, response } => match self.half {
                    Half::Headers => Box::new(std::io::Cursor::new(std::mem::take(headers))),
                    Half::Body => match response.take() {
                        Some(response) => Box::new(std::io::BufReader::new(response)),
                        None => Box::new(std::io::empty()),
                    },
                },

                // Unreachable: `send` leaves `Answered` or `Failed`. An empty
                // reader rather than a panic, because this is a library and
                // `gix` decides the order things are read in.
                Exchange::Pending { .. } => Box::new(std::io::empty()),
            });
        }

        Ok(self.inner.as_mut().expect("just filled in"))
    }
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.ready()?.read(buffer)
    }
}

impl BufRead for Reader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.ready()?.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        if let Some(inner) = self.inner.as_mut() {
            inner.consume(amount);
        }
    }
}

/// The request body `gix` writes into, collected until it is complete.
///
/// Bounded rather than streamed: a fetch's request body is the `want`/`have`
/// negotiation, which for one ref at depth 1 is a few hundred bytes. Streaming
/// it would need a second thread and a pipe to hold the writer open, and this
/// transport has no push to carry.
pub(crate) struct Body(Arc<Mutex<Exchange>>);

impl Write for Body {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if let Exchange::Pending { body, .. } = &mut *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            body.extend_from_slice(buffer);
        }

        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Http for Client {
    type Headers = Reader;
    type ResponseBody = Reader;
    type PostBody = Body;

    fn get(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<GetResponse<Self::Headers, Self::ResponseBody>, HttpError> {
        let exchange = self.exchange(url, headers, None);

        Ok(GetResponse {
            headers: Reader {
                exchange: Arc::clone(&exchange),
                half: Half::Headers,
                inner: None,
            },
            body: Reader {
                exchange,
                half: Half::Body,
                inner: None,
            },
        })
    }

    fn post(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: PostBodyDataKind,
    ) -> Result<PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, HttpError> {
        let exchange = self.exchange(url, headers, Some(body));

        Ok(PostResponse {
            post_body: Body(Arc::clone(&exchange)),
            headers: Reader {
                exchange: Arc::clone(&exchange),
                half: Half::Headers,
                inner: None,
            },
            body: Reader {
                exchange,
                half: Half::Body,
                inner: None,
            },
        })
    }

    fn configure(
        &mut self,
        _config: &dyn std::any::Any,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        // Nothing to configure. The options `gix` would pass here are the ones
        // its own reqwest backend also ignores, plus the two SSL fields this
        // module exists because that backend ignores; taking them and honouring
        // some of them would make `http.sslCAInfo` work in one transport and
        // not the other, which is a worse story than "this crate reads its own
        // configuration and not git's".
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The material every test here plants. If it is ever rendered, something
    /// printed a private key.
    const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

    fn described() -> &'static str {
        "git https://gitlab.internal/acme/config.git main:config.yaml"
    }

    /// A `tls` on an ssh url is a belief about how ssh authenticates a host,
    /// and it is wrong. Saying so beats configuring nothing.
    #[test]
    fn tls_on_a_url_that_has_no_tls_in_it_is_refused() {
        let tls = TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem");

        for url in [
            "ssh://git@github.com/acme/config.git",
            "git@github.com:acme/config.git",
            "file:///srv/config.git",
            "http://gitlab.internal/acme/config.git",
        ] {
            let error = check_scheme(url, &tls).expect_err("{url} has no TLS to configure");

            assert!(
                error.to_string().contains("configures the https"),
                "{error}"
            );
        }

        check_scheme("https://gitlab.internal/acme/config.git", &tls)
            .expect("this one does have TLS in it");

        // And an empty configuration is not a configuration: every url this
        // crate has ever accepted keeps working.
        for url in ["ssh://git@github.com/a.git", "file:///srv/config.git"] {
            check_scheme(url, &TlsConfig::new()).expect("nothing was asked for");
        }
    }

    /// The redaction test every new error path in this crate owes. A private
    /// key that will not parse is the one moment this crate holds one and is
    /// also about to produce a string.
    #[test]
    fn a_bad_client_key_is_refused_without_quoting_it() {
        let tls = TlsConfig::new().with_client_certificate_pem(
            "-----BEGIN CERTIFICATE-----\nnot-a-certificate\n-----END CERTIFICATE-----\n",
            format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
        );

        let error = Client::new(&tls, Duration::from_secs(5), described())
            .expect_err("that is not a usable pair");

        let printed = format!("{error} {error:?}");

        assert!(!printed.contains(PLANTED), "{printed}");
        assert!(printed.contains("not a usable PEM pair"), "{printed}");
    }

    /// The same, for the CA half: a file of nonsense must name the file rather
    /// than quote it.
    #[test]
    fn a_ca_certificate_that_is_not_one_names_the_setting_and_not_the_bytes() {
        let tls = TlsConfig::new().with_ca_certificate_pem(format!("not a pem {PLANTED}"));

        let error = Client::new(&tls, Duration::from_secs(5), described())
            .expect_err("that is not a certificate");

        let printed = format!("{error} {error:?}");

        assert!(!printed.contains(PLANTED), "{printed}");
        assert!(printed.contains("not a PEM certificate"), "{printed}");
    }

    /// A missing file is the ordinary operational mistake — a secret that did
    /// not mount — and it has to name the path.
    #[test]
    fn a_ca_file_that_is_not_there_names_the_path() {
        let tls = TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem");

        let error = Client::new(&tls, Duration::from_secs(5), described())
            .expect_err("the file is not there");

        assert!(
            error.to_string().contains("/nonexistent/private-ca.pem"),
            "{error}"
        );
        assert!(error.to_string().contains(described()), "{error}");
    }

    /// A url carrying a token is quoted into the transport's own failures, and
    /// `reqwest` renders the url it was given into its error text.
    #[test]
    fn a_token_in_a_url_does_not_survive_a_transport_failure() {
        let client = Client::new(&TlsConfig::new(), Duration::from_millis(200), described())
            .expect("an empty configuration builds a plain client");

        let response = client.exchange(
            "https://x-access-token:ghs_hunter2@127.0.0.1:1/acme/config.git/info/refs",
            ["User-Agent: test"],
            None,
        );

        response.lock().unwrap().send();

        let Exchange::Failed(why, _) = &*response.lock().unwrap() else {
            panic!("nothing is listening on port 1");
        };

        assert!(!why.contains("hunter2"), "{why}");
    }
}
