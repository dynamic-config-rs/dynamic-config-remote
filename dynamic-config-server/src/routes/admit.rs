//! Who may ask, and every way this server says no.
//!
//! Admission is one function because the not-an-oracle property is one
//! rule: authorisation is decided from the caller's grants alone, and the
//! section map is never consulted for an application the caller was not
//! granted. Every refusal below is shaped so that "not yours" and "no such
//! thing" cannot be told apart.

use std::sync::Arc;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::{AuditEntry, Outcome};
use crate::auth::Principal;
use crate::server::{Section, Server};

use super::names::is_name;

pub(super) struct Admitted<'a> {
    pub(super) principal: Principal,
    pub(super) section: &'a Arc<Section>,
    pub(super) endpoint: &'static str,
}

pub(super) fn admit<'a>(
    server: &'a Server,
    headers: &HeaderMap,
    application: &str,
    profile: &str,
    endpoint: &'static str,
) -> Result<Admitted<'a>, Box<Response>> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let Some(principal) = server.authenticate(authorization) else {
        return Err(Box::new(refuse(
            server,
            endpoint,
            Outcome::Unauthenticated,
            None,
            None,
        )));
    };

    // Before either segment is looked up *or logged*: a path segment is
    // attacker-controlled text, and an audit line is a place where a newline
    // in one would be a forged second line.
    if !is_name(application) || !is_name(profile) {
        return Err(Box::new(refuse(
            server,
            endpoint,
            Outcome::Malformed,
            Some(principal.name().to_owned()),
            None,
        )));
    }

    let caller = Some(principal.name().to_owned());
    let where_ = Some((application.to_owned(), profile.to_owned()));

    // Authorisation first, and the map is not touched when it fails. This is
    // the whole not-an-oracle property: "not yours" and "no such thing"
    // reach the same line below by the same route.
    if !principal.may_read(application) {
        return Err(Box::new(refuse(
            server,
            endpoint,
            Outcome::NotFound,
            caller,
            where_,
        )));
    }

    let Some(section) = server.section(application, profile) else {
        return Err(Box::new(refuse(
            server,
            endpoint,
            Outcome::NotFound,
            caller,
            where_,
        )));
    };

    Ok(Admitted {
        principal,
        section,
        endpoint,
    })
}

pub(super) fn served(server: &Server, admitted: &Admitted<'_>, generation: u64) {
    server.record(&AuditEntry {
        caller: Some(admitted.principal.name().to_owned()),
        application: Some(admitted.section.application().to_owned()),
        profile: Some(admitted.section.profile().to_owned()),
        endpoint: admitted.endpoint,
        outcome: Outcome::Served,
        generation: Some(generation),
    });
}

pub(super) fn refuse(
    server: &Server,
    endpoint: &'static str,
    outcome: Outcome,
    caller: Option<String>,
    where_: Option<(String, String)>,
) -> Response {
    let (application, profile) = match where_ {
        Some((application, profile)) => (Some(application), Some(profile)),
        None => (None, None),
    };

    server.record(&AuditEntry {
        caller,
        application,
        profile,
        endpoint,
        outcome,
        generation: None,
    });

    match outcome {
        Outcome::Unauthenticated => unauthenticated(),
        _ => not_found(),
    }
}

pub(super) fn unready(server: &Server, admitted: &Admitted<'_>) -> Response {
    server.record(&AuditEntry {
        caller: Some(admitted.principal.name().to_owned()),
        application: Some(admitted.section.application().to_owned()),
        profile: Some(admitted.section.profile().to_owned()),
        endpoint: admitted.endpoint,
        outcome: Outcome::Unavailable,
        generation: None,
    });

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "unavailable" })),
    )
        .into_response()
}

/// Every change stream this process will hold is already held.
///
/// A 503 with a `Retry-After`, so a herd that hit the ceiling backs off
/// instead of spinning — the same courtesy the rate-limiting note asks of
/// whatever ends up doing rate limiting. It is deliberately *not* a 429:
/// nothing about this caller was excessive, the process is full.
pub(super) fn at_capacity(server: &Server, admitted: &Admitted<'_>) -> Response {
    server.record(&AuditEntry {
        caller: Some(admitted.principal.name().to_owned()),
        application: Some(admitted.section.application().to_owned()),
        profile: Some(admitted.section.profile().to_owned()),
        endpoint: admitted.endpoint,
        outcome: Outcome::Unavailable,
        generation: None,
    });

    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        Json(serde_json::json!({ "error": "unavailable" })),
    )
        .into_response()
}

/// A diagnostic endpoint whose sources could not be read at all.
///
/// The category and the key path, not the message: a 500 body is the one
/// place free text would travel furthest, and `ErrorKind` plus the path is
/// what an operator acts on. `/check` is the endpoint that reports *why* a
/// load would fail, and it reports it in its own `failure` field.
pub(super) fn unavailable(
    server: &Server,
    admitted: &Admitted<'_>,
    error: &dynamic_config::Error,
) -> Response {
    server.record(&AuditEntry {
        caller: Some(admitted.principal.name().to_owned()),
        application: Some(admitted.section.application().to_owned()),
        profile: Some(admitted.section.profile().to_owned()),
        endpoint: admitted.endpoint,
        outcome: Outcome::Unavailable,
        generation: None,
    });

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "unavailable",
            "kind": error.kind().as_str(),
            "path": error.path(),
        })),
    )
        .into_response()
}

pub(super) fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(serde_json::json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// The single refusal body. "Not yours" and "no such thing" are this, and
/// nothing else is, so no caller can tell them apart.
pub(super) fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}
