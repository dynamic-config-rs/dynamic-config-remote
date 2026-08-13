//! Serving the router over TLS.
//!
//! # Why this is fifty lines here rather than a dependency
//!
//! `axum-server` is the usual shape and was rejected. The accept loop is the
//! part of a TLS server where the decisions that matter are made, and all of
//! them are this crate's to make:
//!
//! - **A failed handshake must cost one connection, never the listener.**
//!   With client certificates required, failed handshakes are *normal
//!   traffic* — every port scanner and every health checker that does not
//!   know about the certificate produces one.
//! - **A connection must not be able to hold a slot forever.** A client that
//!   opens a socket and sends nothing is free for the client and not free
//!   for the server, so the handshake has a deadline
//!   ([`HANDSHAKE_TIMEOUT`]) — and so do the request headers after it
//!   ([`HEADER_TIMEOUT`]), because a client that completes a handshake and
//!   then goes quiet costs exactly as much as one that never handshook.
//! - **Shutdown must end.** One endpoint here answers with a body that
//!   never ends, so a drain that waits for every response body waits for
//!   every subscriber to leave; [`DRAIN_TIMEOUT`](crate::DRAIN_TIMEOUT) is what keeps a rollout
//!   from hanging on its own change stream.
//! - **A refused handshake must be recorded, and recorded saying nothing.**
//!   It goes through the same [`AuditSink`](crate::AuditSink) as everything
//!   else, as `endpoint=tls outcome=unauthenticated`, with no caller, no
//!   subject and no reason. An operator who has misconfigured a client CA
//!   needs to see *that* handshakes are failing; nobody needs to see whose.
//!
//! `axum-server` would own every one of them and expose none, and its
//! `tls-rustls` feature selects the `aws-lc-rs` provider — a vendored copy
//! of AWS-LC, built with cmake — where this crate wants `ring`, which is
//! already in the workspace's graph. What is used instead is `tokio-rustls`
//! for the handshake and hyper's own HTTP/1 connection for what follows:
//! the same two pieces `axum::serve` uses, with the acceptor spliced in
//! between.
//!
//! The rustls configuration itself is *not* built here: it comes from
//! [`Tls`](crate::tls::Tls), where the key loading, the permission refusal
//! and the client verifier live.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::audit::{AuditEntry, AuditSink, Outcome};
use crate::server::Server;

/// How long a client has to complete a TLS handshake.
///
/// Generous for a handshake and short for a socket somebody is squatting on.
/// It is not a configuration key: a deployment that needs a different number
/// has an idle timeout of its own in front, and this one exists so that a
/// server with no such thing in front is still not held open by a client
/// that connects and says nothing.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connection has, after the handshake, to send request headers.
///
/// The handshake deadline covers TLS and stops there. Without this one, a
/// client that completes a handshake and then sends no request bytes — or
/// drips an incomplete header a byte at a time — holds a socket and a task
/// for as long as it likes, which is the exhaustion
/// [`HANDSHAKE_TIMEOUT`] exists to prevent, moved one step further in.
///
/// It bounds *headers*, not the request as a whole: a stream's response body
/// is meant to run for days, and this deadline stops before the first byte
/// of one is written.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the accept loop waits after an accept error before trying again.
///
/// An `accept` that fails because the process is out of file descriptors
/// fails again immediately, and a loop that retries instantly turns a
/// resource limit into a spin. Ten milliseconds is invisible to a client and
/// is the difference between busy-waiting and waiting.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(10);

/// Serves `router` over TLS until `shutdown` completes.
///
/// The TLS material and the audit sink both come from `server`, which is the
/// same one the router was built over; `router` is passed separately so that
/// an embedder can wrap it — a layer of its own, a different fallback — the
/// way it can with [`axum::serve()`].
///
/// Shutdown is graceful in both halves: the listener stops accepting, and
/// every connection already open is told to finish the request it is serving
/// and close. A config server is restarted by a rollout, and dropping the
/// fetch a pod is making at that moment would make a rollout look like a
/// configuration failure to whoever is starting up beside it.
///
/// Graceful, and bounded: a connection that has not finished within
/// [`DRAIN_TIMEOUT`](crate::DRAIN_TIMEOUT) is dropped. An open change stream has no end of its
/// own, so an unbounded drain would be a rollout that waits for its
/// subscribers rather than the other way round.
///
/// # Errors
///
/// Only what the listener itself reports. A connection that fails — a
/// handshake that was refused, a client that went away — ends that
/// connection and nothing else.
///
/// # Panics
///
/// If `server` is not configured for TLS. Callers reach this function
/// through [`Server::tls`], which is `None` exactly when that is so.
pub async fn serve_tls<S>(
    listener: TcpListener,
    router: Router,
    server: &Server,
    shutdown: S,
) -> io::Result<()>
where
    // No `'static`: the shutdown future is awaited here rather than spawned,
    // so it may borrow whatever the caller has to hand — a `CancellationToken`
    // it owns, a channel on its own stack.
    S: Future<Output = ()> + Send,
{
    let tls = server
        .tls()
        .expect("serve_tls is reached through Server::tls, which is None without a `[server.tls]`");
    let acceptor = TlsAcceptor::from(tls.server_config());
    let audit = server.audit_sink();

    // `false` until the shutdown future completes, at which point every
    // connection task sees it and finishes what it is doing.
    let (closing, closed) = watch::channel(false);
    let mut connections = JoinSet::new();

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        connections.spawn(connection(
                            acceptor.clone(),
                            stream,
                            router.clone(),
                            Arc::clone(&audit),
                            closed.clone(),
                        ));
                    }
                    // Per-connection, not per-server: a client that went away
                    // between the SYN and the accept, or a descriptor limit.
                    Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                }
            }
            // Reaping is part of the loop rather than a task of its own, so
            // that a long-lived server does not accumulate a `JoinSet` entry
            // per connection it has ever served.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    // The listener first: a client connecting during shutdown gets a refused
    // connection, which it will retry, rather than a connection that is
    // accepted and then closed unanswered.
    drop(listener);
    let _ = closing.send(true);

    while connections.join_next().await.is_some() {}

    Ok(())
}

/// One connection: handshake, then HTTP until it ends or shutdown does.
async fn connection(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    router: Router,
    audit: Arc<dyn AuditSink>,
    mut closed: watch::Receiver<bool>,
) {
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;

    let stream = match handshake {
        Ok(Ok(stream)) => stream,
        // Both failures are recorded identically and say nothing about the
        // certificate that was or was not presented. A refused handshake is
        // a caller that presented no usable credential, which is an outcome
        // this log already has a word for.
        Ok(Err(_)) | Err(_) => {
            audit.record(&AuditEntry {
                caller: None,
                application: None,
                profile: None,
                endpoint: "tls",
                outcome: Outcome::Unauthenticated,
                generation: None,
            });

            return;
        }
    };

    let service = TowerToHyperService::new(router);

    let mut builder = http1::Builder::new();

    // The timer is not a detail: hyper reads deadlines from the one the
    // builder was given, and a `header_read_timeout` set without one is
    // silently not enforced.
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT);

    let connection = builder.serve_connection(TokioIo::new(stream), service);

    tokio::pin!(connection);

    tokio::select! {
        _ = connection.as_mut() => {}
        _ = closed.changed() => {
            // Finish the request in flight, refuse to start another, close
            // — but not for longer than `DRAIN_TIMEOUT`, because a stream's
            // body has no end of its own and would otherwise hold the
            // rollout open until its client happened to leave.
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(crate::DRAIN_TIMEOUT, connection).await;
        }
    }
}
