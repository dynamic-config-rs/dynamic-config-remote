//! The HTTP surface.
//!
//! Two path segments carry the whole addressing scheme: the first is the
//! application, the second is the profile, and what hangs off them is this
//! crate's own vocabulary.
//!
//! # One endpoint returns values
//!
//! `GET /{application}/{profile}` is the handover — the resolved document,
//! secrets included, which is what a config server is *for*. Every other
//! endpoint returns shape, provenance or counts: paths without what is at
//! them, an explanation with every value replaced by `***`, a check report
//! that names keys and origins, a status that is timestamps and numbers,
//! and a metrics scrape that is the same numbers with a label naming the
//! section they belong to.
//!
//! That line is drawn once, here, and it is drawn wider than the library
//! draws it. `explain` in the library deliberately *does* carry values —
//! you asked, at a terminal, for one path. Over a socket the same answer is
//! a value that has left the process for a reason nobody weighed, so the
//! server pushes every explanation through
//! [`Explanation::redacted`](dynamic_config::Explanation::redacted) rather
//! than only the paths it believes are secret. Reusing the library's
//! redaction rather than writing a second one is the point; applying it
//! unconditionally is the server's own decision.
//!
//! # It will not be an oracle
//!
//! A caller that may not read `billing` and a caller asking for an
//! application nobody serves get the same 404, with the same body, having
//! done the same work: authorisation is decided from the caller's grants
//! alone, and the section map is never consulted for an application the
//! caller was not granted. There is nothing to time and nothing to read.
//!
//! Seven files, one concern each: the router here, then the endpoints
//! grouped by what they answer — liveness, metrics, documents,
//! diagnostics, the change stream — with admission and the refusal
//! responses in [`admit`](admit) because every handler goes through them,
//! and the two path predicates in [`names`](names) because
//! [`ServerConfig::validate`](crate::ServerConfig) uses one of them too.

mod admit;
mod diagnostics;
mod documents;
mod health;
mod metrics;
mod names;
mod stream;

pub(crate) use names::is_name;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::server::Server;

use admit::not_found;
use diagnostics::{check, explain};
use documents::{document, paths, status};
use health::{healthz, readyz};
use metrics::metrics;
use stream::stream;

/// The router, over a started [`Server`].
///
/// Everything is a `GET`: this server serves configuration and changes
/// nothing, so there is no verb here that could.
pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/{application}/{profile}", get(document))
        .route("/{application}/{profile}/paths", get(paths))
        .route("/{application}/{profile}/check", get(check))
        .route("/{application}/{profile}/status", get(status))
        .route("/{application}/{profile}/stream", get(stream))
        .route("/{application}/{profile}/explain/{path}", get(explain))
        // So that a path this server does not route answers exactly like a
        // section it will not serve: same status, same body.
        .fallback(|| async { not_found() })
        .with_state(server)
}
