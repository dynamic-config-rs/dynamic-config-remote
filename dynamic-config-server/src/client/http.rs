//! The small HTTP/1.1 client the two client-side endpoints share.
//!
//! Not a client *library*: `hyper::client::conn::http1` is the connection
//! `axum::serve` is the other half of, and this module is the fifty lines
//! between a socket and a `Response`. The reasoning is `serve.rs`'s, in the
//! other direction — the decisions that matter here are this crate's:
//!
//! - **One connection per request, and no pool.** A document is fetched when
//!   a generation moves, which is minutes or hours apart; a pool would keep a
//!   socket open to the program that holds every service's secrets for the
//!   sake of a handshake nobody is waiting for.
//! - **A response body is bounded.** A client that trusts a server to send a
//!   finite document is a client that can be made to allocate until it dies,
//!   and the server this talks to is exactly the thing an attacker who has
//!   got that far would be impersonating.
//! - **One deadline covers the whole attempt** — connect, TLS handshake,
//!   request *and* body ([`Budget`]). A fetch that hangs is a reload that
//!   never happens, and the loop above has no other way to notice. A
//!   deadline per step would be neither: three steps of ten seconds is a
//!   thirty-second fetch, and a body with no deadline at all is a server
//!   that answers with headers and then stops writing — which costs an
//!   attacker one socket and costs the client every reload after it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dynamic_config::Error;
use http_body_util::{BodyExt as _, Empty, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{ACCEPT, AUTHORIZATION, HOST};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

/// Where the server is, taken apart once so a request does not have to
/// re-parse it.
#[derive(Debug, Clone)]
pub(super) struct Endpoint {
    pub(super) secure: bool,
    pub(super) host: String,
    pub(super) port: u16,
    /// A path this server's router is mounted under, or `""`. Empty for the
    /// binary, non-empty for a service that mounted [`router`](crate::router)
    /// inside its own application.
    pub(super) prefix: String,
}

impl Endpoint {
    /// `scheme://host[:port][/prefix]`, taken apart.
    ///
    /// # Errors
    ///
    /// A [`Error::remote`] naming what is wrong with the URL. Note the one
    /// refusal that is not about syntax: a `user:password@` authority is
    /// **refused rather than sent**, because this server takes one credential
    /// shape and it is not that one — a URL that carries a password would
    /// have it dropped silently, which reads as a working configuration until
    /// the first 401.
    pub(super) fn parse(url: &str, described: &str) -> Result<Self, Error> {
        let refuse = |why: &str| Error::remote(format!("{described}: {why}"));

        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(refuse("the URL needs an `http://` or `https://` scheme"));
        };

        let secure = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            _ => return Err(refuse("the URL's scheme has to be `http` or `https`")),
        };

        let (authority, prefix) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };

        if authority.contains('@') {
            return Err(refuse(
                "the URL carries a `user:password@` authority, which this server does not \
                 accept as a credential; put the bearer token in `with_token` instead",
            ));
        }

        let (host, port) = split_authority(authority, secure).ok_or_else(|| {
            refuse("the URL's host and port are not `host`, `host:port` or `[v6]:port`")
        })?;

        if host.is_empty() {
            return Err(refuse("the URL names no host"));
        }

        Ok(Self {
            secure,
            host,
            port,
            // A trailing slash would make every path a double one. The empty
            // prefix is the binary's case and stays empty.
            prefix: prefix.trim_end_matches('/').to_owned(),
        })
    }

    /// What goes in the `Host` header: the authority as written, port
    /// included unless it is the scheme's default.
    fn authority(&self) -> String {
        let bracketed = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };

        if (self.secure && self.port == 443) || (!self.secure && self.port == 80) {
            bracketed
        } else {
            format!("{bracketed}:{}", self.port)
        }
    }

    /// The path for one of this server's endpoints, under any mount prefix.
    pub(super) fn path(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }
}

/// `host`, `host:port` or `[v6]:port`, with the scheme's default port.
fn split_authority(authority: &str, secure: bool) -> Option<(String, u16)> {
    let default = if secure { 443 } else { 80 };

    // A bracketed IPv6 literal is the one shape `rsplit_once(':')` gets
    // wrong: every one of its own colons looks like a port separator.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;

        return match tail {
            "" => Some((host.to_owned(), default)),
            _ => Some((host.to_owned(), tail.strip_prefix(':')?.parse().ok()?)),
        };
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
        None => Some((authority.to_owned(), default)),
    }
}

/// What is left of one fetch's deadline.
///
/// A fetch is four waits — connect, handshake, request, body — and the
/// promise `with_timeout` makes is about the fetch, not about each of them.
/// So the budget is started once and every step is bounded by what remains:
/// a slow connect leaves the body less time rather than adding to the total.
///
/// A budget that has run out yields `Duration::ZERO`, which times out
/// immediately and reports as the step that had nothing left.
#[derive(Debug, Clone, Copy)]
pub(super) struct Budget {
    until: Instant,
}

impl Budget {
    pub(super) fn starting(timeout: Duration) -> Self {
        Self {
            until: Instant::now() + timeout,
        }
    }

    pub(super) fn left(self) -> Duration {
        self.until.saturating_duration_since(Instant::now())
    }
}

/// One open connection, and the task driving it.
///
/// hyper's client connection is two halves: a `SendRequest` the caller holds
/// and a future that has to be polled for anything to move. Dropping this
/// aborts the second, which is what makes a dropped `watch` future close its
/// socket rather than leave a task polling one nobody reads.
pub(super) struct Connection {
    sender: SendRequest<Empty<Bytes>>,
    driver: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Connection {
    /// Connects, over TLS when `tls` is `Some`.
    ///
    /// # Errors
    ///
    /// If the address will not resolve, the connection is refused, the TLS
    /// handshake fails, or any of it outlives what `budget` has left. **No message here
    /// carries certificate or key material**: a handshake failure is
    /// rustls's own sentence, which names the certificate's problem and not
    /// its contents.
    pub(super) async fn open(
        endpoint: &Endpoint,
        tls: Option<&Arc<rustls::ClientConfig>>,
        budget: Budget,
        described: &str,
    ) -> Result<Self, Error> {
        let stream = deadline(
            budget.left(),
            TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
            described,
            "connecting",
        )
        .await?
        .map_err(|error| Error::remote(format!("{described}: connecting: {error}")))?;

        // A configuration document is one small request and one small
        // response; waiting 40ms for a second write that is not coming is
        // the whole latency of a reload.
        let _ = stream.set_nodelay(true);

        let Some(config) = tls else {
            return Self::handshake(stream, described).await;
        };

        let name = ServerName::try_from(endpoint.host.clone()).map_err(|_| {
            Error::remote(format!(
                "{described}: `{}` is neither a DNS name nor an IP address, so no certificate \
                 could be checked against it",
                endpoint.host
            ))
        })?;
        let stream = deadline(
            budget.left(),
            TlsConnector::from(Arc::clone(config)).connect(name, stream),
            described,
            "the TLS handshake",
        )
        .await?
        .map_err(|error| {
            Error::remote(format!("{described}: the TLS handshake failed: {error}"))
        })?;

        Self::handshake(stream, described).await
    }

    async fn handshake<S>(stream: S, described: &str) -> Result<Self, Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| Error::remote(format!("{described}: {error}")))?;

        Ok(Self {
            sender,
            // The connection future owns the socket and the parser; the
            // result is the connection ending, which the response body
            // reports in its own terms.
            driver: tokio::spawn(async move {
                let _ = connection.await;
            }),
        })
    }

    /// One `GET`, with the bearer token if there is one.
    ///
    /// # Errors
    ///
    /// If the request cannot be built or the server does not answer. A token
    /// that is not a legal header value is refused **without being quoted**.
    pub(super) async fn get(
        &mut self,
        endpoint: &Endpoint,
        path: &str,
        token: Option<&str>,
        accept: &str,
        budget: Budget,
        described: &str,
    ) -> Result<Response<Incoming>, Error> {
        let mut request = Request::builder()
            .method("GET")
            .uri(path)
            .header(HOST, endpoint.authority())
            .header(ACCEPT, accept);

        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        // The builder's error is dropped rather than reported, and the token
        // is the reason: the one way to reach it here is a header value the
        // token made illegal, and `http`'s own message is where a credential
        // would travel if that ever changed.
        let request = request.body(Empty::<Bytes>::new()).map_err(|_| {
            Error::remote(format!(
                "{described}: the request could not be built; a bearer token has to be a legal \
                 HTTP header value"
            ))
        })?;

        deadline(
            budget.left(),
            self.sender.send_request(request),
            described,
            "the request",
        )
        .await?
        .map_err(|error| Error::remote(format!("{described}: {error}")))
    }
}

/// The whole body, refused past `limit` and past what `budget` has left.
///
/// The deadline is the half that is easy to miss: a server that sends
/// headers and then stops writing holds this future open for as long as it
/// likes, and the bound on *size* never comes into it because the bytes
/// never arrive.
///
/// # Errors
///
/// If the body cannot be read, is longer than `limit`, or does not arrive
/// within what the fetch has left.
pub(super) async fn body(
    response: Response<Incoming>,
    limit: usize,
    budget: Budget,
    described: &str,
) -> Result<Vec<u8>, Error> {
    let collected = deadline(
        budget.left(),
        Limited::new(response.into_body(), limit).collect(),
        described,
        "the response body",
    )
    .await?
    .map_err(|_| {
        Error::remote(format!(
            "{described}: the response body could not be read, or was longer than \
                 {limit} bytes"
        ))
    })?;

    Ok(collected.to_bytes().to_vec())
}

/// What a status other than 200 means to the loop above.
///
/// The split is the one a watch loop acts on: [`ErrorKind::Auth`] will not
/// fix itself while the loop waits, and [`ErrorKind::Remote`] may.
///
/// **A 404 is on the credential side**, which is worth stating because it
/// looks like the other one. This server answers "you may not read that" and
/// "there is no such thing" with the same 404, deliberately — so a client
/// cannot tell them apart either, and *neither* is fixed by retrying: one is
/// a grant this caller does not have, the other is a section this server does
/// not serve, and both are somebody editing a configuration file.
///
/// [`ErrorKind::Auth`]: dynamic_config::ErrorKind::Auth
/// [`ErrorKind::Remote`]: dynamic_config::ErrorKind::Remote
pub(super) fn refused(status: StatusCode, described: &str) -> Error {
    match status {
        StatusCode::UNAUTHORIZED => Error::auth(format!(
            "{described}: the server refused the credential (401); this client presented no \
             bearer token, or one the server's roster does not have"
        )),
        StatusCode::FORBIDDEN => Error::auth(format!(
            "{described}: the server refused the credential (403)"
        )),
        StatusCode::NOT_FOUND => Error::auth(format!(
            "{described}: the server answered 404, which it uses for both `this caller may not \
             read that` and `no such application and profile` — check the grant and the \
             section, because waiting will not change either"
        )),
        _ => Error::remote(format!("{described}: the server answered {status}")),
    }
}

/// `future`, or a [`Error::remote`] naming what timed out.
async fn deadline<F>(
    timeout: Duration,
    future: F,
    described: &str,
    what: &str,
) -> Result<F::Output, Error>
where
    F: std::future::Future,
{
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        Error::remote(format!(
            "{described}: {what} did not finish within {timeout:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str) -> Endpoint {
        Endpoint::parse(url, "config-server").expect("a URL this crate accepts")
    }

    #[test]
    fn a_url_is_taken_apart_into_scheme_host_port_and_prefix() {
        let plain = endpoint("http://config.internal:8080");

        assert!(!plain.secure);
        assert_eq!(plain.host, "config.internal");
        assert_eq!(plain.port, 8080);
        assert_eq!(plain.prefix, "");
        assert_eq!(plain.path("/billing/prod"), "/billing/prod");

        let mounted = endpoint("https://config.internal/config/");

        assert!(mounted.secure);
        assert_eq!(mounted.port, 443, "https defaults to 443");
        assert_eq!(mounted.prefix, "/config");
        assert_eq!(mounted.path("/billing/prod"), "/config/billing/prod");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_colons_and_its_port() {
        let six = endpoint("http://[::1]:8080/");

        assert_eq!(six.host, "::1");
        assert_eq!(six.port, 8080);
        assert_eq!(six.authority(), "[::1]:8080");

        assert_eq!(endpoint("http://[::1]").port, 80);
    }

    #[test]
    fn the_host_header_drops_a_default_port_and_keeps_any_other() {
        assert_eq!(
            endpoint("https://config.internal").authority(),
            "config.internal"
        );
        assert_eq!(
            endpoint("http://config.internal").authority(),
            "config.internal"
        );
        assert_eq!(
            endpoint("https://config.internal:8443").authority(),
            "config.internal:8443"
        );
    }

    /// The refusal that is about the threat model rather than about syntax:
    /// this server takes one credential shape, and a URL is not it.
    #[test]
    fn a_password_in_the_url_is_refused_rather_than_dropped() {
        let error = Endpoint::parse("https://user:hunter2@config.internal", "config-server")
            .expect_err("a URL credential is not a credential here");

        assert!(error.to_string().contains("with_token"), "{error}");
        assert!(
            !error.to_string().contains("hunter2"),
            "the refusal quoted the password: {error}"
        );
    }

    #[test]
    fn a_url_without_a_usable_scheme_or_host_is_refused() {
        for url in [
            "config.internal:8080",
            "ftp://config.internal",
            "https://",
            "http://config.internal:not-a-port",
        ] {
            assert!(
                Endpoint::parse(url, "config-server").is_err(),
                "`{url}` should not parse"
            );
        }
    }

    #[test]
    fn a_status_says_whether_waiting_could_help() {
        use dynamic_config::ErrorKind;

        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            assert_eq!(
                refused(status, "config-server").kind(),
                ErrorKind::Auth,
                "{status} is a configuration problem, not a transient one"
            );
        }

        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
        ] {
            assert_eq!(refused(status, "config-server").kind(), ErrorKind::Remote);
        }
    }
}
