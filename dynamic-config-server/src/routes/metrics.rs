//! `GET /metrics` — authenticated, and scoped to the caller's grants.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use dynamic_config::telemetry::Exposition;

use crate::audit::{AuditEntry, Outcome};
use crate::server::Server;

use super::admit::refuse;

/// The Prometheus exposition format's content type. Version 0.0.4 is the
/// text format every scraper reads, and naming it is what makes a scraper
/// parse the body rather than store it.
const EXPOSITION: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` — one scrape, covering **the sections this caller may
/// read** and no others.
///
/// # Why this one is authenticated when `/healthz` and `/readyz` are not
///
/// Those two answer a boolean and say nothing else: not how many sections
/// there are, not which one is unhappy. That is precisely what lets them be
/// open. A useful metrics endpoint cannot be that — a series that cannot
/// name the section it describes is a series nobody can alert on — so
/// `/metrics` carries application and profile labels, and an application
/// name is exactly what the not-an-oracle property exists to withhold. An
/// open `/metrics` would enumerate every service the fleet configures to
/// anyone who could reach the port.
///
/// So it takes the same bearer token as everything else and, having taken
/// it, reports only what that principal is already entitled to ask for one
/// section at a time through `/status`. A scraper is a client like any
/// other: give it a token and grant it the applications it should see.
/// Prometheus has read `authorization` and `bearer_token_file` from its
/// scrape configuration for years, so this costs a deployment two lines.
///
/// The alternative — an open endpoint with no labels, counting sections in
/// aggregate — was rejected: it says less than `/readyz` already does and
/// still cannot be alerted on.
///
/// # Cardinality
///
/// Bounded by the served set, not by the documents. `6 × sections` series
/// at a scrape, and `19 × sections` over a process's life once the two
/// fixed enums behind the `reason` and `kind` labels are counted. **No key
/// path, file name or value can become a label**: every sample here comes
/// from [`ConfigStatus`](dynamic_config::ConfigStatus), which holds none of
/// them, and the two labels this crate adds are an application and a
/// profile that the server's own configuration named and that `is_name`
/// has already bounded.
pub(super) async fn metrics(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let Some(principal) = server.authenticate(authorization) else {
        return refuse(&server, "metrics", Outcome::Unauthenticated, None, None);
    };

    let mut exposition = Exposition::new();

    for section in server.sections() {
        // The same grant check every other endpoint makes, and for the same
        // reason: a caller learns the shape of its own sections and of
        // nothing else. A principal granted nothing gets a well-formed
        // empty scrape rather than a refusal — it is somebody, and there is
        // nothing to tell it.
        if !principal.may_read(section.application()) {
            continue;
        }

        exposition.add_with(
            &[
                ("application", section.application()),
                ("profile", section.profile()),
            ],
            &section.status(),
        );
    }

    server.record(&AuditEntry {
        caller: Some(principal.name().to_owned()),
        // A scrape is about the server, not about one section: naming a
        // section here would be naming several.
        application: None,
        profile: None,
        endpoint: "metrics",
        outcome: Outcome::Served,
        generation: None,
    });

    ([(header::CONTENT_TYPE, EXPOSITION)], exposition.render()).into_response()
}
