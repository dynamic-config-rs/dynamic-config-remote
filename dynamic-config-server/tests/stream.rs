//! The change stream: what it sends, what it refuses, and what it costs.
//!
//! The three questions that kept this endpoint out of 0.5 are the three this
//! suite is arranged around — resumption, memory under a fleet-wide
//! reconnect, and backpressure — and each is asserted rather than argued,
//! because the answer to all three is a property of *what an event carries*
//! and a property is a thing a test can hold.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{client, fixture, section, start, Fixture};
use dynamic_config_server::ServerConfig;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

/// An open subscription: the response, and the body it is still writing to.
struct Subscription {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Body,
}

impl Subscription {
    /// The next event, or a panic if none arrives.
    ///
    /// Five seconds because an install and a wake are microseconds; this is
    /// a deadlock detector, not a timing assertion.
    async fn event(&mut self) -> String {
        let frame = tokio::time::timeout(Duration::from_secs(5), self.body.frame())
            .await
            .expect("an event arrives promptly after an install")
            .expect("the stream has not ended")
            .expect("a frame rather than an error");

        String::from_utf8(
            frame
                .into_data()
                .expect("an event is a data frame")
                .to_vec(),
        )
        .expect("every event is text")
    }

    /// Whether an event arrives within `patience`. For asserting silence.
    async fn quiet_for(&mut self, patience: Duration) -> bool {
        tokio::time::timeout(patience, self.body.frame())
            .await
            .is_err()
    }
}

/// Opens a subscription, optionally resuming from a generation.
async fn subscribe(
    fixture: &Fixture,
    uri: &str,
    token: Option<&str>,
    resume: Option<u64>,
) -> Subscription {
    let mut request = Request::builder().method("GET").uri(uri);

    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(resume) = resume {
        request = request.header("last-event-id", resume.to_string());
    }

    let response = fixture
        .router
        .clone()
        .oneshot(request.body(Body::empty()).expect("a well-formed request"))
        .await
        .expect("the router is infallible");

    Subscription {
        status: response.status(),
        headers: response.headers().clone(),
        body: response.into_body(),
    }
}

/// One reload of a served section, which is what moves a generation.
fn reload(fixture: &Fixture, application: &str, profile: &str) {
    fixture
        .server
        .section(application, profile)
        .expect("the fixture serves it")
        .reload()
        .expect("the fixture's files load");
}

// ---------------------------------------------------------------------------
// What an event is
// ---------------------------------------------------------------------------

/// The stream opens by saying where the caller stands, so a client does not
/// have to guess whether it is behind.
#[tokio::test]
async fn a_stream_opens_with_the_generation_it_is_at() {
    let fixture = fixture();
    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    assert_eq!(subscription.status, StatusCode::OK);
    assert_eq!(
        subscription
            .headers
            .get("content-type")
            .expect("a content type")
            .to_str()
            .expect("it is text"),
        "text/event-stream",
        "a scraper and a browser both key off this"
    );

    let event = subscription.event().await;

    assert!(event.contains("id: 1"), "{event}");
    assert!(event.contains("event: generation"), "{event}");
    assert!(
        event.contains(r#""application":"billing""#) && event.contains(r#""profile":"prod""#),
        "{event}"
    );
    assert!(event.contains(r#""generation":1"#), "{event}");
}

/// An install is one event, and the event is a number.
///
/// **No value, and no key path either.** The document endpoint is the one
/// endpoint that serves values; a stream that carried them would be a second
/// one, with a body that outlives the request that authorised it.
#[tokio::test]
async fn an_install_is_one_event_carrying_a_number_and_nothing_else() {
    let fixture = fixture();
    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    let opening = subscription.event().await;
    assert!(opening.contains(r#""generation":1"#), "{opening}");

    reload(&fixture, "billing", "prod");

    let event = subscription.event().await;

    assert!(event.contains("id: 2"), "{event}");
    assert!(event.contains(r#""generation":2"#), "{event}");
    assert!(
        !event.contains("hunter2"),
        "a value reached the stream: {event}"
    );
    assert!(
        !event.contains("password") && !event.contains("pool"),
        "a key path reached the stream: {event}"
    );
}

// ---------------------------------------------------------------------------
// Resumption
// ---------------------------------------------------------------------------

/// `Last-Event-ID` at the current generation opens *silent*: the client is
/// up to date, and telling it so again would be an event that means nothing.
#[tokio::test]
async fn a_client_that_is_up_to_date_is_told_nothing_until_something_happens() {
    let fixture = fixture();
    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        Some(1),
    )
    .await;

    assert!(
        subscription.quiet_for(Duration::from_millis(200)).await,
        "the caller already has generation 1"
    );

    reload(&fixture, "billing", "prod");

    let event = subscription.event().await;
    assert!(event.contains(r#""generation":2"#), "{event}");
}

/// The resumption question item 23 parked — *what about a client that
/// reconnects past the end of the buffer* — answered by not having a buffer.
/// A generation is monotonic, so the current one subsumes every one before
/// it: five installs missed are one event, not five, and nothing was lost
/// because there was never a queue to lose it from.
#[tokio::test]
async fn a_client_that_is_far_behind_gets_one_event_rather_than_a_replay() {
    let fixture = fixture();

    for _ in 0..5 {
        reload(&fixture, "billing", "prod");
    }

    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        Some(1),
    )
    .await;

    let event = subscription.event().await;
    assert!(event.contains(r#""generation":6"#), "{event}");
    assert!(event.contains("id: 6"), "{event}");

    assert!(
        subscription.quiet_for(Duration::from_millis(200)).await,
        "the four generations between are not replayed: they are subsumed"
    );
}

/// A `Last-Event-ID` that is not a number is ignored rather than refused.
/// Something in the path mangling a header must not turn a reconnect into a
/// failure; one redundant event is the cheaper mistake.
#[tokio::test]
async fn an_unparsable_last_event_id_is_ignored_rather_than_refused() {
    let fixture = fixture();

    let mut request = Request::builder()
        .method("GET")
        .uri("/billing/prod/stream")
        .header("authorization", format!("Bearer {}", common::BILLING_TOKEN))
        .header("last-event-id", "not-a-number");

    request = request.header("accept", "text/event-stream");

    let response = fixture
        .router
        .clone()
        .oneshot(request.body(Body::empty()).expect("a well-formed request"))
        .await
        .expect("the router is infallible");

    assert_eq!(response.status(), StatusCode::OK);

    let mut subscription = Subscription {
        status: response.status(),
        headers: response.headers().clone(),
        body: response.into_body(),
    };

    assert!(subscription.event().await.contains(r#""generation":1"#));
}

/// A generation counts installs *since this process started*, so a restart
/// puts every section back at 1 while a reconnecting `EventSource` still
/// sends the id the previous process handed it. That id is higher than
/// anything the new process will emit for a long time, and a
/// greater-than test would open silent and stay silent — the client
/// missing every change until the new process had reloaded past the old
/// number, which for a long-lived id is the rest of the connection's life.
///
/// A resumed generation the section is not at is news, whichever side of
/// it it falls on.
#[tokio::test]
async fn a_client_resuming_from_a_previous_process_is_not_left_silent() {
    let fixture = fixture();

    // What a client would hold from a server that had been up for a while.
    // This fixture is a fresh process: its sections are at generation 1.
    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        Some(50),
    )
    .await;

    let event = subscription.event().await;
    assert!(
        event.contains(r#""generation":1"#),
        "a restarted server must tell a resuming client where it actually \
         is: {event}"
    );
}

// ---------------------------------------------------------------------------
// It is an endpoint like every other one
// ---------------------------------------------------------------------------

/// The not-an-oracle property, on the endpoint that would be the easiest
/// place to lose it: a subscription to somebody else's section and a
/// subscription to a section nobody serves are byte-identical.
#[tokio::test]
async fn a_subscription_to_a_section_that_is_not_yours_is_the_same_404() {
    let fixture = fixture();

    let (not_yours, not_yours_body) = fixture
        .get("/billing/prod/stream", Some(common::PAYROLL_TOKEN))
        .await;
    let (no_such_thing, no_such_thing_body) = fixture
        .get("/nothing/prod/stream", Some(common::PAYROLL_TOKEN))
        .await;
    let (no_such_profile, no_such_profile_body) = fixture
        .get("/payroll/nothing/stream", Some(common::PAYROLL_TOKEN))
        .await;

    assert_eq!(not_yours, StatusCode::NOT_FOUND);
    assert_eq!(no_such_thing, StatusCode::NOT_FOUND);
    assert_eq!(no_such_profile, StatusCode::NOT_FOUND);
    assert_eq!(not_yours_body, no_such_thing_body);
    assert_eq!(not_yours_body, no_such_profile_body);
}

#[tokio::test]
async fn a_subscription_without_a_credential_is_refused() {
    let fixture = fixture();

    let (status, _) = fixture.get("/billing/prod/stream", None).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The audit log records the *subscription*, once. A line per install per
/// connection would drown the log that matters, and the events say no more
/// than a `/status` poll would.
#[tokio::test]
async fn the_audit_log_records_the_subscription_and_not_every_event() {
    let fixture = fixture();
    let mut subscription = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    let _ = subscription.event().await;

    for _ in 0..3 {
        reload(&fixture, "billing", "prod");
        let _ = subscription.event().await;
    }

    let lines = fixture.log_lines();
    let subscriptions = lines
        .iter()
        .filter(|line| line.contains("endpoint=stream"))
        .count();

    assert_eq!(subscriptions, 1, "{lines:?}");
    assert!(
        lines[0].contains("caller=billing-pod") && lines[0].contains("generation=1"),
        "the line names who subscribed and where they started: {lines:?}"
    );
    assert!(
        !fixture.joined_log().contains("hunter2"),
        "no value reaches the log: {}",
        fixture.joined_log()
    );
}

// ---------------------------------------------------------------------------
// What a thousand pods cost
// ---------------------------------------------------------------------------

/// The memory claim, as a number rather than a sentence: an event's size is
/// independent of the document behind it, because the event does not carry
/// the document or a diff of it. Two sections whose names are the same
/// length, one holding a byte and one holding a hundred kilobytes, produce
/// events of exactly the same size.
#[tokio::test]
async fn an_event_is_the_same_size_whatever_the_section_holds() {
    let directory = tempfile::tempdir().expect("a temporary directory");

    let small = directory.path().join("small.toml");
    std::fs::write(&small, "[small]\nk = 'v'\n").expect("writable");

    let large = directory.path().join("large.toml");
    let mut document = String::from("[large]\n");
    for key in 0..2_000 {
        document.push_str(&format!("key{key} = '{}'\n", "x".repeat(50)));
    }
    std::fs::write(&large, &document).expect("writable");
    assert!(
        document.len() > 100_000,
        "the large section really is large"
    );

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![
            section("small", "prod", small.display().to_string()),
            section("large", "prod", large.display().to_string()),
        ],
        clients: vec![client(
            "both-pod",
            common::BILLING_TOKEN,
            &["small", "large"],
        )],
        ..ServerConfig::default()
    };

    let fixture = start(directory, config);

    let mut on_small = subscribe(
        &fixture,
        "/small/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;
    let mut on_large = subscribe(
        &fixture,
        "/large/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    let from_small = on_small.event().await;
    let from_large = on_large.event().await;

    assert_eq!(
        from_small.len(),
        from_large.len(),
        "an event carries a generation, not a document:\n{from_small}\n{from_large}"
    );
}

/// The ceiling: a process holds a stated number of streams and refuses the
/// next with a `Retry-After`, so a herd backs off instead of spinning. And a
/// connection that ends gives its place back — the permit lives in the
/// response body, so however the connection ends, dropping it releases.
#[tokio::test]
async fn the_ceiling_refuses_the_next_one_and_a_closed_stream_gives_its_place_back() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nhost = 'db.internal'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        max_stream_connections: 2,
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let fixture = start(directory, config);

    let first = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;
    let second = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(fixture.server.open_streams(), 2);

    let refused = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;

    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused
            .headers
            .get("retry-after")
            .expect("a herd has to be told when to come back")
            .to_str()
            .expect("it is text"),
        "5"
    );

    drop(second);

    assert_eq!(
        fixture.server.open_streams(),
        1,
        "dropping the body releases"
    );

    let third = subscribe(
        &fixture,
        "/billing/prod/stream",
        Some(common::BILLING_TOKEN),
        None,
    )
    .await;
    assert_eq!(third.status, StatusCode::OK);

    drop(first);
    drop(third);

    assert_eq!(fixture.server.open_streams(), 0);
}

/// Streaming off is not a different refusal. A deployment that does not want
/// long-lived connections sets the ceiling to zero, and the endpoint answers
/// exactly as a path this server does not have.
#[tokio::test]
async fn a_server_with_streaming_turned_off_answers_the_same_404() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nhost = 'db.internal'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        max_stream_connections: 0,
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let fixture = start(directory, config);

    let (status, body) = fixture
        .get("/billing/prod/stream", Some(common::BILLING_TOKEN))
        .await;
    let (missing, missing_body) = fixture
        .get(
            "/billing/prod/nothing-like-this",
            Some(common::BILLING_TOKEN),
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(body, missing_body);
}

/// A fleet-wide reconnect, at a scale a test can hold: every connection gets
/// the install, and each costs a handle rather than a copy of the document.
#[tokio::test]
async fn a_hundred_subscriptions_all_see_one_install() {
    let fixture = fixture();

    let mut subscriptions = Vec::new();

    for _ in 0..100 {
        let mut subscription = subscribe(
            &fixture,
            "/billing/prod/stream",
            Some(common::BILLING_TOKEN),
            None,
        )
        .await;
        assert_eq!(subscription.status, StatusCode::OK);
        let _ = subscription.event().await;
        subscriptions.push(subscription);
    }

    assert_eq!(fixture.server.open_streams(), 100);

    reload(&fixture, "billing", "prod");

    for subscription in &mut subscriptions {
        let event = subscription.event().await;
        assert!(event.contains(r#""generation":2"#), "{event}");
    }

    drop(subscriptions);
    assert_eq!(fixture.server.open_streams(), 0);
}
