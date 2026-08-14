//! Liveness and readiness. Unauthenticated, and so they say nothing: not
//! how many sections there are, not which one is unhappy. An operator
//! reads `/{application}/{profile}/status` for that, with a credential.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::server::Server;

pub(super) async fn healthz() -> Response {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response()
}

pub(super) async fn readyz(State(server): State<Arc<Server>>) -> Response {
    let ready = server.is_ready();
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (code, Json(serde_json::json!({ "ready": ready }))).into_response()
}
