//! Every endpoint, driven through the real router — including the cases
//! that are the point of the crate: no credential, the wrong credential, a
//! section that is not yours, and a path that is not a path.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{anonymous, client, fixture, section, start};
use dynamic_config_server::ServerConfig;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// The happy paths.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_document_endpoint_serves_values_to_the_client_that_owns_them() {
    let fixture = fixture();

    let (status, body) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["application"], "billing");
    assert_eq!(body["profile"], "prod");
    assert_eq!(body["generation"], 1);
    assert_eq!(body["config"]["host"], "db.internal");
    assert_eq!(body["config"]["password"], "hunter2");
    assert_eq!(body["config"]["pool"]["max_size"], 8);
}

#[tokio::test]
async fn the_paths_endpoint_names_every_key_and_carries_no_value() {
    let fixture = fixture();

    let (status, body) = fixture
        .json("/billing/prod/paths", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["paths"],
        serde_json::json!(["host", "password", "pool.max_size"])
    );
    assert!(
        !body.to_string().contains("hunter2"),
        "the shape endpoint served a value: {body}"
    );
}

#[tokio::test]
async fn explain_names_the_winning_layer_and_redacts_every_value() {
    let fixture = fixture();

    let (status, body) = fixture
        .json(
            "/billing/prod/explain/pool.max_size",
            Some(common::BILLING_TOKEN),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], "pool.max_size");
    assert_eq!(body["winner"], "file");

    let rows = body["rows"].as_array().expect("explain returns rows");
    let winner = rows
        .iter()
        .find(|row| row["layer"] == "file")
        .expect("the file layer has a row");

    // The origin is the useful half and is exactly the half that is safe.
    assert!(
        winner["origin"]
            .as_str()
            .expect("a supplying layer names its origin")
            .contains("billing.toml"),
        "{body}"
    );
    // The value is not, and it is `***` even though `8` is not a secret: over
    // a socket the server redacts every path rather than guessing which.
    for row in rows {
        assert!(
            row["value"] == serde_json::Value::Null || row["value"] == "***",
            "a row rendered something other than `***` or nothing: {row}"
        );
    }
}

#[tokio::test]
async fn check_reports_where_each_key_comes_from_and_that_it_would_load() {
    let fixture = fixture();

    let (status, body) = fixture
        .json("/billing/prod/check", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["clean"], true);
    assert_eq!(body["failure"], serde_json::Value::Null);

    let resolved = body["resolved"].as_array().expect("check resolves keys");
    let paths: Vec<&str> = resolved
        .iter()
        .map(|row| row["path"].as_str().expect("a path"))
        .collect();

    assert_eq!(paths, ["host", "password", "pool.max_size"]);
    assert!(
        !body.to_string().contains("hunter2"),
        "a check report served a value: {body}"
    );
}

#[tokio::test]
async fn status_reports_the_live_generation_and_that_it_is_healthy() {
    let fixture = fixture();

    let (status, body) = fixture
        .json("/billing/prod/status", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["generation"], 1);
    assert_eq!(body["healthy"], true);
    assert_eq!(body["ready"], true);
    assert_eq!(body["consecutive_failures"], 0);
    assert_eq!(body["last_reason"], "initial");
    assert_eq!(body["last_failure"], serde_json::Value::Null);
    assert!(body["stale_for_seconds"].is_number(), "{body}");
}

#[tokio::test]
async fn two_profiles_of_one_application_are_two_documents() {
    let fixture = fixture();

    let (_, prod) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;
    let (status, staging) = fixture
        .json("/billing/staging", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(prod["config"]["host"], "db.internal");
    assert_eq!(staging["config"]["host"], "db.staging");
}

#[tokio::test]
async fn liveness_and_readiness_need_no_credential_and_say_nothing_else() {
    let fixture = fixture();

    let (status, body) = fixture.json("/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "status": "ok" }));

    let (status, body) = fixture.json("/readyz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({ "ready": true }),
        "readiness must not say how many sections there are, or which"
    );
}

// ---------------------------------------------------------------------------
// The negative cases.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_with_no_credential_is_401_and_asks_for_a_bearer_token() {
    let fixture = fixture();

    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/billing/prod")
                .body(Body::empty())
                .expect("a well-formed request"),
        )
        .await
        .expect("the router is infallible");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );
}

#[tokio::test]
async fn every_endpoint_refuses_an_uncredentialed_caller() {
    let fixture = fixture();

    for uri in [
        "/billing/prod",
        "/billing/prod/paths",
        "/billing/prod/check",
        "/billing/prod/status",
        "/billing/prod/explain/host",
        "/billing/prod/stream",
        "/metrics",
    ] {
        let (status, _) = fixture.get(uri, None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED, "`{uri}` was reachable");
    }
}

#[tokio::test]
async fn a_token_nobody_holds_is_401_rather_than_404() {
    let fixture = fixture();

    let (status, _) = fixture
        .get("/billing/prod", Some("not-a-token-0123456789abcdefghij"))
        .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unusable credential is about the caller, not about the section"
    );
}

/// The property the whole design turns on: a caller cannot learn whether
/// something exists by asking for it.
#[tokio::test]
async fn a_section_that_is_not_yours_is_indistinguishable_from_one_that_is_not_there() {
    let fixture = fixture();

    // `payroll/prod` exists and is somebody else's.
    let (not_yours_status, not_yours) = fixture
        .get("/payroll/prod", Some(common::BILLING_TOKEN))
        .await;
    // `ledger/prod` is served by nobody at all.
    let (absent_status, absent) = fixture
        .get("/ledger/prod", Some(common::BILLING_TOKEN))
        .await;
    // `billing/nowhere` is an application the caller *does* hold, at a
    // profile nothing serves — the one 404 the caller is entitled to.
    let (wrong_profile_status, wrong_profile) = fixture
        .get("/billing/nowhere", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(not_yours_status, StatusCode::NOT_FOUND);
    assert_eq!(absent_status, StatusCode::NOT_FOUND);
    assert_eq!(wrong_profile_status, StatusCode::NOT_FOUND);

    assert_eq!(not_yours, absent, "the bodies must be byte-identical");
    assert_eq!(not_yours, wrong_profile);
    assert_eq!(not_yours, r#"{"error":"not_found"}"#);
    assert!(
        !not_yours.contains("payroll") && !not_yours.contains("billing"),
        "the refusal named a section: {not_yours}"
    );
}

#[tokio::test]
async fn the_same_404_covers_every_endpoint_of_a_section_that_is_not_yours() {
    let fixture = fixture();

    for uri in [
        "/payroll/prod",
        "/payroll/prod/paths",
        "/payroll/prod/check",
        "/payroll/prod/status",
        "/payroll/prod/stream",
        "/payroll/prod/explain/host",
    ] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "`{uri}`");
        assert_eq!(body, r#"{"error":"not_found"}"#, "`{uri}`");
    }
}

#[tokio::test]
async fn a_malformed_application_or_profile_is_the_same_404() {
    let fixture = fixture();

    for uri in [
        // A percent-encoded newline, which would otherwise forge an audit line.
        "/billing%0Aadmin/prod",
        // A percent-encoded space.
        "/billing/pr%20od",
        // Traversal, refused by the leading-character rule rather than by a
        // special case.
        "/..%2f..%2fetc/prod",
        // Longer than any application name anybody wants.
        "/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/prod",
    ] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "`{uri}`");
        assert_eq!(body, r#"{"error":"not_found"}"#, "`{uri}`");
    }
}

/// The shape check runs on the *decoded* segment, which is the only place
/// it is worth running: a check against the raw path would pass anything an
/// attacker could spell in percent-encoding.
#[tokio::test]
async fn the_shape_check_sees_the_decoded_segment() {
    let fixture = fixture();

    let (status, body) = fixture
        .json("/bill%69ng/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "`%69` is `i`, so this is a request for `billing`"
    );
    assert_eq!(body["application"], "billing");
}

#[tokio::test]
async fn a_malformed_explain_path_is_the_same_404() {
    let fixture = fixture();

    for uri in [
        "/billing/prod/explain/pool..max_size",
        "/billing/prod/explain/.host",
        "/billing/prod/explain/ho%20st",
    ] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "`{uri}`");
        assert_eq!(body, r#"{"error":"not_found"}"#, "`{uri}`");
    }
}

#[tokio::test]
async fn a_path_this_server_does_not_route_answers_exactly_like_one_it_will_not_serve() {
    let fixture = fixture();

    for uri in ["/", "/billing", "/billing/prod/bogus", "/a/b/c/d/e"] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "`{uri}`");
        assert_eq!(body, r#"{"error":"not_found"}"#, "`{uri}`");
    }
}

/// Every route is a `GET`. A config server that could be written to is a
/// different product with a different threat model.
#[tokio::test]
async fn nothing_here_accepts_a_write() {
    let fixture = fixture();

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (status, _) = fixture
            .send(
                Request::builder()
                    .method(method)
                    .uri("/billing/prod")
                    .header("authorization", format!("Bearer {}", common::BILLING_TOKEN))
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await;

        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
}

// ---------------------------------------------------------------------------
// Anonymous access, which exists and is still scoped.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_anonymous_caller_reads_only_what_the_anonymous_client_was_granted() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let demo = directory.path().join("demo.toml");
    let billing = directory.path().join("billing.toml");
    std::fs::write(&demo, "[demo]\ngreeting = 'hello'\n").expect("writable");
    std::fs::write(&billing, "[billing]\npassword = 'hunter2'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        allow_anonymous: true,
        sections: vec![
            section("demo", "dev", demo.display().to_string()),
            section("billing", "prod", billing.display().to_string()),
        ],
        clients: vec![
            anonymous("anonymous", &["demo"]),
            client("billing-pod", common::BILLING_TOKEN, &["billing"]),
        ],
        ..ServerConfig::default()
    };
    let fixture = start(directory, config);

    let (status, body) = fixture.json("/demo/dev", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["greeting"], "hello");

    let (status, body) = fixture.get("/billing/prod", None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "anonymous is a principal with grants, not a way past them"
    );
    assert_eq!(body, r#"{"error":"not_found"}"#);
}

// ---------------------------------------------------------------------------
// The reason to front a store at all.
// ---------------------------------------------------------------------------

/// A bad edit upstream must not become an outage: the previous document
/// keeps serving, and the server says so where a pipeline will see it.
#[tokio::test]
async fn a_bad_edit_keeps_the_last_good_document_and_reports_unready() {
    let fixture = fixture();

    fixture.write(
        "billing.toml",
        "[billing]\nhost = 'db.internal'\nthis is not toml",
    );

    let section = fixture
        .server
        .section("billing", "prod")
        .expect("the fixture serves it");
    section.reload().expect_err("the file no longer parses");

    let (status, body) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the previous document keeps serving"
    );
    assert_eq!(body["config"]["host"], "db.internal");
    assert_eq!(body["generation"], 1, "nothing new was installed");

    let (status, body) = fixture
        .json("/billing/prod/status", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["healthy"], false);
    assert_eq!(body["ready"], false);
    assert_eq!(body["consecutive_failures"], 1);
    assert_eq!(body["last_failure"]["kind"], "parse");

    let (status, body) = fixture.json("/readyz", None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a section fronting a broken source is not ready"
    );
    assert_eq!(body, serde_json::json!({ "ready": false }));
}

/// And it recovers: the point of counting *consecutive* failures is that one
/// success clears them.
#[tokio::test]
async fn a_good_edit_after_a_bad_one_installs_and_the_server_is_ready_again() {
    let fixture = fixture();
    let section = fixture
        .server
        .section("billing", "prod")
        .expect("the fixture serves it");

    fixture.write("billing.toml", "not toml at all [[[");
    section.reload().expect_err("the file no longer parses");

    fixture.write(
        "billing.toml",
        "[billing]\nhost = 'db.new'\npassword = 'hunter2'\n",
    );
    section.reload().expect("the file parses again");

    let (status, body) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["host"], "db.new");
    assert_eq!(body["generation"], 2);

    let (status, body) = fixture.json("/readyz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "ready": true }));
}

// ---------------------------------------------------------------------------
// Metrics.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metrics_needs_a_credential_and_says_so_the_way_everything_else_does() {
    let fixture = fixture();

    let (status, body) = fixture.get("/metrics", None).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an open /metrics would enumerate every application the fleet configures"
    );
    assert_eq!(body, r#"{"error":"unauthenticated"}"#);

    let (status, _) = fixture
        .get("/metrics", Some("not-a-configured-token"))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn metrics_covers_the_callers_own_sections_and_no_others() {
    let fixture = fixture();

    let (status, headers, body) = fixture
        .get_with_headers("/metrics", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8"),
        "a scraper parses the body only if the version is named"
    );

    assert!(
        body.contains(r#"dynamic_config_installs_total{application="billing",profile="prod"} 1"#),
        "{body}"
    );
    assert!(
        body.contains(
            r#"dynamic_config_installs_total{application="billing",profile="staging"} 1"#
        ),
        "{body}"
    );
    assert!(
        !body.contains("payroll"),
        "a caller learns the shape of its own sections and of nothing else: {body}"
    );
}

/// The grant is the whole scope: the payroll client scrapes payroll, and
/// the two scrapes together are the server — which no single caller sees.
#[tokio::test]
async fn each_caller_scrapes_its_own_half() {
    let fixture = fixture();

    let (_, payroll) = fixture.get("/metrics", Some(common::PAYROLL_TOKEN)).await;

    assert!(payroll.contains(r#"application="payroll""#), "{payroll}");
    assert!(!payroll.contains("billing"), "{payroll}");
}

/// A client granted an application nobody scrapes for is still somebody:
/// an empty scrape, not a refusal. There is nothing to tell it.
#[tokio::test]
async fn a_caller_with_nothing_to_see_gets_an_empty_scrape() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("payroll.toml");
    std::fs::write(&file, "[payroll]\nhost = 'payroll.internal'\n").expect("a writable directory");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![section("payroll", "prod", file.display().to_string())],
        clients: vec![
            client("payroll-pod", common::PAYROLL_TOKEN, &["payroll"]),
            client("watcher", common::BILLING_TOKEN, &[]),
        ],
        ..ServerConfig::default()
    };
    let fixture = start(directory, config);

    let (status, body) = fixture.get("/metrics", Some(common::BILLING_TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "", "no grants, no series, no refusal either");
}

/// A failed reload upstream is what a scrape exists to show: the section
/// keeps serving, and the numbers say it is stale and unhealthy.
#[tokio::test]
async fn a_refused_reload_shows_up_as_failures_and_staleness() {
    let fixture = fixture();

    fixture.write(
        "billing.toml",
        "[billing]\nhost = 'db.internal'\nthis is not toml",
    );
    let section = fixture
        .server
        .section("billing", "prod")
        .expect("the fixture serves it");
    section.reload().expect_err("the file no longer parses");

    let (_, body) = fixture.get("/metrics", Some(common::BILLING_TOKEN)).await;

    assert!(
        body.contains(
            r#"dynamic_config_consecutive_failures{application="billing",profile="prod"} 1"#
        ),
        "{body}"
    );
    assert!(
        body.contains(r#"dynamic_config_last_failure_info{application="billing",profile="prod",kind="parse"} 1"#),
        "{body}"
    );
    assert!(
        body.contains(
            r#"dynamic_config_last_success_seconds{application="billing",profile="prod"}"#
        ),
        "staleness is the series an alert is written against: {body}"
    );
}

// ---------------------------------------------------------------------------
// A file that carries no section header
// ---------------------------------------------------------------------------

/// The file another tool wrote. A config server is routinely pointed at
/// one, and such a file has no reason to carry a header this server
/// invented — `whole_document = true` says so, per section, and everything
/// downstream is unchanged.
#[tokio::test]
async fn a_section_may_read_files_that_carry_no_header() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("billing.json");
    std::fs::write(&path, r#"{"host": "db.internal", "pool": {"max_size": 8}}"#)
        .expect("the temporary directory is writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![dynamic_config_server::SectionConfig {
            application: "billing".to_owned(),
            profile: "prod".to_owned(),
            files: vec![path.display().to_string()],
            env_prefix: None,
            whole_document: true,
        }],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let fixture = start(directory, config);

    let (status, body) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["host"], "db.internal");
    assert_eq!(body["config"]["pool"]["max_size"], 8);
}
