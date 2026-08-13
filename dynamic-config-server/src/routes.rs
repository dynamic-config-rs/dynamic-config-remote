//! The HTTP surface.
//!
//! Spring Cloud Config Server's shape, because a deployment's mental model
//! transfers for free: the first path segment is the application, the second
//! is the profile, and what hangs off them is this crate's own vocabulary.
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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dynamic_config::telemetry::Exposition;
use dynamic_config::Changes;
use futures_core::Stream;
use serde::Serialize;

use crate::audit::{AuditEntry, Outcome};
use crate::auth::Principal;
use crate::document::Document;
use crate::server::{Section, Server, StreamPermit};

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

// ---------------------------------------------------------------------------
// Liveness and readiness. Unauthenticated, and so they say nothing: not how
// many sections there are, not which one is unhappy. An operator reads
// `/{application}/{profile}/status` for that, with a credential.
// ---------------------------------------------------------------------------

async fn healthz() -> Response {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response()
}

async fn readyz(State(server): State<Arc<Server>>) -> Response {
    let ready = server.is_ready();
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (code, Json(serde_json::json!({ "ready": ready }))).into_response()
}

// ---------------------------------------------------------------------------
// Metrics. Authenticated, and scoped to the caller's grants — see the
// handler.
// ---------------------------------------------------------------------------

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
async fn metrics(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
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

// ---------------------------------------------------------------------------
// The served endpoints.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DocumentBody<'a> {
    application: &'a str,
    profile: &'a str,
    generation: u64,
    config: &'a serde_json::Value,
}

/// The resolved document. **The one endpoint that returns values.**
async fn document(
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
async fn paths(
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
async fn status(
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

// ---------------------------------------------------------------------------
// The change stream.
// ---------------------------------------------------------------------------

/// How long a stream may be silent before a comment goes down it.
///
/// Not a configuration key: it is the interval an idle TCP connection needs
/// to survive the proxies these deployments sit behind, and a deployment
/// that wants a different one has an idle timeout of its own to set.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `GET /{application}/{profile}/stream` — one `text/event-stream` per
/// caller, one event per install.
///
/// # What an event carries, and what it deliberately does not
///
/// A generation number, and the application and profile it belongs to:
///
/// ```text
/// id: 7
/// event: generation
/// data: {"application":"billing","profile":"prod","generation":7}
/// ```
///
/// **Not the document, and not the changed paths either.** The document
/// endpoint is the one endpoint that serves values, and a stream that
/// carried them would be a second one — with a different lifetime, a
/// different failure mode, and a body that outlives the request that
/// authorised it. Changed *paths* would leak nothing `/paths` does not
/// already tell the same caller, but they would have to be diffed per
/// install and carried per connection, which is the memory bound this
/// design exists to avoid. So the event says *something landed, here is its
/// number*, and the client re-fetches the endpoint it was already using.
///
/// That choice is what makes the rest of it simple:
///
/// - **Resumption is a comparison, not a buffer.** A generation is
///   monotonic and the current one subsumes every one before it, so
///   `Last-Event-ID: 6` against a section at 9 is one event carrying 9 —
///   nothing was missed, because there was never anything to miss. There is
///   no ring of recent events, so there is no bound to choose and no
///   "reconnected past the end of it" case to answer.
/// - **Backpressure needs no policy.** The stream carries a level rather
///   than a log: a client that stops reading is simply not polled, and when
///   it is polled again it gets the *latest* generation. Nothing queues, so
///   nothing has to be dropped.
/// - **Memory is flat.** Per connection: one `Changes` handle (an `Arc`
///   clone and a `u64`), one registered waker, and two short strings. No
///   document, no diff, no channel. A thousand pods reconnecting after a
///   restart cost a thousand of that and one shared install.
///
/// # It is an endpoint like every other one
///
/// Authenticated, authorised against the caller's grants, and refused with
/// the same 404 as everything else — a subscription to a section the caller
/// may not read does the same work and returns the same body as a
/// subscription to a section nobody serves. The audit log records the
/// *subscription*, once, with the generation it opened at; the events after
/// it say no more than a `/status` poll would, and a line per install per
/// connection would drown the log that matters.
async fn stream(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "stream") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    // A deployment that turns streaming off does not serve this path, and
    // says so with the body it says everything else with. Checked after
    // admission so that the answer costs an unauthorised caller exactly
    // what every other 404 costs it.
    if !server.streams_enabled() {
        return refuse(
            &server,
            "stream",
            Outcome::NotFound,
            Some(admitted.principal.name().to_owned()),
            Some((
                admitted.section.application().to_owned(),
                admitted.section.profile().to_owned(),
            )),
        );
    }

    let Some(permit) = server.open_stream() else {
        return at_capacity(&server, &admitted);
    };

    let section = Arc::clone(admitted.section);
    let generation = section.generation();

    served(&server, &admitted, generation);

    let stream = Generations {
        application: section.application().to_owned(),
        profile: section.profile().to_owned(),
        changes: section.changes(),
        resume: last_event_id(&headers),
        sent: None,
        section,
        _permit: permit,
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// `Last-Event-ID`, as the generation a client says it already has.
///
/// Anything that is not a number is *ignored* rather than refused: the
/// header is echoed back by browsers and proxies from whatever the last
/// event carried, and a reconnect that fails because something in the path
/// mangled a header is a worse failure than one extra event.
fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// One connection's stream of generations.
///
/// Everything it holds is fixed-size. That is the property the whole design
/// turns on, so it is worth naming the fields: two strings from the server's
/// own configuration, an `Arc` to the section, a `Changes` handle, the last
/// number sent, the number the client claimed, and the permit that releases
/// its place in the ceiling on drop.
struct Generations {
    application: String,
    profile: String,
    section: Arc<Section>,
    changes: Changes<Document>,
    /// The last generation this connection emitted.
    sent: Option<u64>,
    /// What the client's `Last-Event-ID` claimed, if anything.
    resume: Option<u64>,
    _permit: StreamPermit,
}

impl Generations {
    /// Whether `generation` is news to this connection.
    fn is_news(&self, generation: u64) -> bool {
        // Zero is "nothing has ever been installed here", which is not an
        // event: a section serves nothing until it has a document, and
        // `/readyz` is where that is reported.
        if generation == 0 {
            return false;
        }

        match self.sent {
            Some(sent) => generation > sent,
            // The opening event. A client that said where it was gets one
            // unless the section is exactly where it said; a client that
            // said nothing gets one either way, so that it starts knowing
            // where it stands rather than having to guess.
            //
            // *Different*, not *greater*. A generation counts installs
            // since this process started, so a restart puts the section
            // back at 1 while a reconnecting `EventSource` still sends the
            // `Last-Event-ID` the previous process gave it. Under a
            // greater-than test, a client resuming from 50 would be told
            // nothing by the new process until it had reloaded fifty times
            // — silently missing every change in between, for the life of
            // the connection. A number that is not the one the client
            // holds is news, whichever side of it it falls.
            None => match self.resume {
                Some(resumed) => generation != resumed,
                None => true,
            },
        }
    }

    fn event(&self, generation: u64) -> Event {
        let data = serde_json::json!({
            "application": self.application,
            "profile": self.profile,
            "generation": generation,
        })
        .to_string();

        // The id *is* the generation, which is what makes `Last-Event-ID`
        // resumption a comparison rather than a lookup.
        Event::default()
            .id(generation.to_string())
            .event("generation")
            .data(data)
    }
}

impl Stream for Generations {
    // Infallible: nothing between an install and an event can fail. A
    // connection ends because the client went away, which is the body being
    // dropped rather than an error travelling down it.
    type Item = Result<Event, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            let generation = this.section.generation();

            if this.is_news(generation) {
                this.sent = Some(generation);

                return Poll::Ready(Some(Ok(this.event(generation))));
            }

            // A fresh future per poll on purpose: `changed()` keeps no state
            // of its own — the generation it has seen lives in the `Changes`
            // handle and the check-register-check protocol lives in the
            // cell — so re-creating it is the same future, and it saves
            // this type from having to be self-referential.
            let changed = this.changes.changed();

            match std::pin::pin!(changed).poll(context) {
                // Something installed. Round again to read the generation it
                // became, rather than trusting the snapshot: the number the
                // event carries is the one `/status` and the document
                // endpoint report, and it comes from the same load.
                Poll::Ready(_) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

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
async fn check(
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
async fn explain(
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

// ---------------------------------------------------------------------------
// Admission: authenticate, then check the request's shape, then authorise,
// then — and only then — look anything up.
// ---------------------------------------------------------------------------

struct Admitted<'a> {
    principal: Principal,
    section: &'a Arc<Section>,
    endpoint: &'static str,
}

fn admit<'a>(
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

fn served(server: &Server, admitted: &Admitted<'_>, generation: u64) {
    server.record(&AuditEntry {
        caller: Some(admitted.principal.name().to_owned()),
        application: Some(admitted.section.application().to_owned()),
        profile: Some(admitted.section.profile().to_owned()),
        endpoint: admitted.endpoint,
        outcome: Outcome::Served,
        generation: Some(generation),
    });
}

fn refuse(
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

fn unready(server: &Server, admitted: &Admitted<'_>) -> Response {
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
fn at_capacity(server: &Server, admitted: &Admitted<'_>) -> Response {
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
fn unavailable(
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

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(serde_json::json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// The single refusal body. "Not yours" and "no such thing" are this, and
/// nothing else is, so no caller can tell them apart.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

/// An application or profile: a first character that is a letter or a digit,
/// then letters, digits, `.`, `_` and `-`, to 64.
///
/// Narrow on purpose. It bounds what can reach the audit log, it rejects
/// `..` and the empty segment without a special case for either, and there
/// is no application name anybody wants that it refuses.
///
/// `pub(crate)` so that [`ServerConfig::validate`](crate::ServerConfig)
/// refuses at startup exactly what the handlers refuse at request time: a
/// section this rejects would load, start and answer nothing.
pub(crate) fn is_name(value: &str) -> bool {
    let mut characters = value.chars();

    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 64
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A dotted key path: non-empty segments of letters, digits, `_` and `-`,
/// to 256 characters overall.
fn is_key_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_narrow_and_rejects_traversal_and_newlines() {
        assert!(is_name("billing"));
        assert!(is_name("billing-api"));
        assert!(is_name("billing.api_2"));

        assert!(!is_name(""), "the empty segment");
        assert!(!is_name(".."), "traversal");
        assert!(!is_name(".hidden"), "a leading dot");
        assert!(!is_name("bill ing"), "a space");
        assert!(!is_name("billing\nadmin"), "a forged audit line");
        assert!(!is_name("bill/ing"), "a separator");
        assert!(!is_name(&"a".repeat(65)), "unbounded length");
    }

    #[test]
    fn a_key_path_is_dotted_and_has_no_empty_segments() {
        assert!(is_key_path("port"));
        assert!(is_key_path("pool.max_size"));
        assert!(is_key_path("a-b.c-d"));

        assert!(!is_key_path(""));
        assert!(!is_key_path("."));
        assert!(!is_key_path("pool."));
        assert!(!is_key_path(".pool"));
        assert!(!is_key_path("pool..max"));
        assert!(!is_key_path("pool max"));
        assert!(!is_key_path(&"a".repeat(257)));
    }
}
