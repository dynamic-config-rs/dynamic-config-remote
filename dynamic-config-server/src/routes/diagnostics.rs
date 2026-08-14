//! What would load, and where each value comes from — the two endpoints
//! that answer about a section without handing it over.
//!
//! `explain` is where the line is drawn wider than the library draws it:
//! every value is `***`, not only the ones a schema called secret.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::server::Server;

use crate::audit::Outcome;

use super::admit::{admit, refuse, served, unavailable};
use super::names::is_key_path;

#[derive(Serialize)]
struct CheckBody<'a> {
    application: &'a str,
    profile: &'a str,
    clean: bool,
    resolved: Vec<ResolvedRow>,
    unknown: Vec<UnknownRow>,
    failure: Option<String>,
}

#[derive(Serialize)]
struct ResolvedRow {
    path: String,
    origin: String,
}

#[derive(Serialize)]
struct UnknownRow {
    path: String,
    suggestion: Option<String>,
}

/// Would the *next* load succeed, and where would each key come from.
///
/// Re-reads the sources — that is the question it answers — so it runs on
/// the blocking pool rather than on a request worker. It is therefore the
/// one endpoint here that costs I/O per request; bounding how often an
/// authorised caller may ask is the job of the thing in front, which is
/// where rate limiting lives — per-caller limiting needs a view of every
/// caller, and this process is one replica of several.
///
/// `failure` is the library's own error text, which is value-free by policy
/// and by `dynamic-config/tests/security.rs`. It can name a *file*, which
/// is the point of the endpoint and is a file the caller is authorised for.
///
/// `unknown` is **always empty here**, and that is a property of the shape
/// rather than a gap in the plumbing: unknown-key detection compares the
/// resolved keys against a struct's field names, and a config server does
/// not know its callers' structs — that is the whole reason the served
/// document is schemaless. A caller that wants the check run against its
/// own type runs it in its own process, where the type is.
pub(super) async fn check(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "check") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    let sources = admitted.section.sources();
    let report = match dynamic_config::off_thread(move || sources.check()).await {
        Ok(report) => report,
        Err(error) => return unavailable(&server, &admitted, &error),
    };

    let generation = admitted.section.generation();
    served(&server, &admitted, generation);

    Json(CheckBody {
        application: admitted.section.application(),
        profile: admitted.section.profile(),
        clean: report.is_clean(),
        resolved: report
            .resolved
            .into_iter()
            .map(|resolved| ResolvedRow {
                path: resolved.path,
                origin: resolved.origin.to_string(),
            })
            .collect(),
        unknown: report
            .unknown
            .into_iter()
            .map(|unknown| UnknownRow {
                path: unknown.path,
                suggestion: unknown.suggestion,
            })
            .collect(),
        failure: report.failure,
    })
    .into_response()
}

#[derive(Serialize)]
struct ExplainBody<'a> {
    application: &'a str,
    profile: &'a str,
    path: String,
    winner: Option<&'static str>,
    rows: Vec<ExplainRow>,
}

#[derive(Serialize)]
struct ExplainRow {
    layer: &'static str,
    origin: Option<String>,
    /// `***` where the layer supplies something, `null` where it does not —
    /// the shape [`Explanation::redacted`] leaves behind. Never a value.
    value: Option<String>,
}

/// Why a value is what it is, without saying what it is.
///
/// The feature nobody else has: an operator asks the *server* which layer
/// won, from a laptop, without shelling into a pod. Every value is `***`.
pub(super) async fn explain(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile, path)): Path<(String, String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "explain") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    // After admission, so a caller who may not read this section learns
    // nothing from the shape of a path it was never going to get.
    if !is_key_path(&path) {
        return refuse(
            &server,
            "explain",
            Outcome::Malformed,
            Some(admitted.principal.name().to_owned()),
            Some((
                admitted.section.application().to_owned(),
                admitted.section.profile().to_owned(),
            )),
        );
    }

    let sources = admitted.section.sources();
    let asked = path.clone();
    let explanation = match dynamic_config::off_thread(move || sources.explain(&asked)).await {
        // Redacted unconditionally: see this module's documentation.
        Ok(explanation) => explanation.redacted(),
        Err(error) => return unavailable(&server, &admitted, &error),
    };

    let generation = admitted.section.generation();
    served(&server, &admitted, generation);

    Json(ExplainBody {
        application: admitted.section.application(),
        profile: admitted.section.profile(),
        path,
        winner: explanation.winner().map(|row| row.layer),
        rows: explanation
            .rows()
            .iter()
            .map(|row| ExplainRow {
                layer: row.layer,
                origin: row.origin.as_ref().map(ToString::to_string),
                value: row.value.clone(),
            })
            .collect(),
    })
    .into_response()
}
