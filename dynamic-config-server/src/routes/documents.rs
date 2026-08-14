//! The endpoints that answer with a section's contents: the document
//! itself, the keys it holds, and what is true of it right now.
//!
//! One of the three returns values, and it is the only endpoint in this
//! crate that does.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::server::Server;

use super::admit::{admit, served, unready};

#[derive(Serialize)]
struct DocumentBody<'a> {
    application: &'a str,
    profile: &'a str,
    generation: u64,
    config: &'a serde_json::Value,
}

/// The resolved document. **The one endpoint that returns values.**
pub(super) async fn document(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "document") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    // One coherent pair: the number never describes an install this
    // document is not already at least at. See `Section::installed`.
    let Some((generation, document)) = admitted.section.installed() else {
        return unready(&server, &admitted);
    };

    served(&server, &admitted, generation);

    Json(DocumentBody {
        application: admitted.section.application(),
        profile: admitted.section.profile(),
        generation,
        config: document.as_json(),
    })
    .into_response()
}

#[derive(Serialize)]
struct PathsBody<'a> {
    application: &'a str,
    profile: &'a str,
    generation: u64,
    paths: Vec<String>,
}

/// Which keys exist, without what is at them.
///
/// The endpoint a dashboard or a schema check wants: it answers "does this
/// deployment set `pool.max_size`" without becoming a way to read it.
pub(super) async fn paths(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "paths") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    let Some((generation, document)) = admitted.section.installed() else {
        return unready(&server, &admitted);
    };

    served(&server, &admitted, generation);

    Json(PathsBody {
        application: admitted.section.application(),
        profile: admitted.section.profile(),
        generation,
        paths: document.leaf_paths(),
    })
    .into_response()
}

#[derive(Serialize)]
struct StatusBody<'a> {
    application: &'a str,
    profile: &'a str,
    generation: u64,
    ready: bool,
    healthy: bool,
    consecutive_failures: u32,
    stale_for_seconds: Option<f64>,
    last_reason: Option<&'static str>,
    last_failure: Option<FailureBody>,
}

#[derive(Serialize)]
struct FailureBody {
    kind: &'static str,
    path: String,
    seconds_ago: f64,
}

/// The operational surface: which generation is live, when it landed, why,
/// and how the reloads since have gone.
///
/// A handful of atomic loads and no I/O, so a scrape per second costs
/// nothing. The reason is the *category* — `file-changed` rather than the
/// path that changed — because a metric dimension carrying a filesystem path
/// has unbounded cardinality and because the path is not the caller's
/// business.
pub(super) async fn status(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "status") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    let status = admitted.section.status();
    let generation = status.generation;
    served(&server, &admitted, generation);

    Json(StatusBody {
        application: admitted.section.application(),
        profile: admitted.section.profile(),
        generation,
        ready: admitted.section.is_ready(),
        healthy: status.is_healthy(),
        consecutive_failures: status.consecutive_failures,
        stale_for_seconds: status.stale_for().map(|elapsed| elapsed.as_secs_f64()),
        last_reason: status
            .last_reason
            .as_ref()
            .map(dynamic_config::ReloadReason::as_str),
        last_failure: status.last_failure.as_ref().map(|failure| FailureBody {
            kind: failure.kind.as_str(),
            path: failure.path.clone(),
            seconds_ago: failure.at.elapsed().as_secs_f64(),
        }),
    })
    .into_response()
}
