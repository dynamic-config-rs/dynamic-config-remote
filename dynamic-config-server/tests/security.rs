//! The planted-secret suite, ported from `dynamic-config/tests/security.rs`.
//!
//! The library's rule is that a value never appears in a diagnostic. A server
//! *exists* to serve values, so the rule becomes a boundary rather than a
//! prohibition: one endpoint returns them, everything else must not, and the
//! log must not, ever. Every assertion here is that boundary, planted with a
//! value distinctive enough that finding it anywhere is unambiguous.

mod common;

use axum::http::StatusCode;
use common::{client, fixture, section, start};
use dynamic_config_server::ServerConfig;

/// The value planted in `billing.toml` by the shared fixture.
const SECRET: &str = "hunter2";
/// And the one the staging profile carries, so a test that drives both does
/// not have to think about which is which.
const STAGING_SECRET: &str = "staging-secret";

#[tokio::test]
async fn the_document_endpoint_is_the_only_one_that_serves_the_value() {
    let fixture = fixture();

    let (status, document) = fixture
        .get("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        document.contains(SECRET),
        "the handover endpoint has to hand it over, or the server is pointless"
    );

    for uri in [
        "/billing/prod/paths",
        "/billing/prod/check",
        "/billing/prod/status",
        "/billing/prod/explain/password",
        "/billing/prod/explain/host",
        "/metrics",
    ] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::OK, "`{uri}`");
        assert!(
            !body.contains(SECRET),
            "`{uri}` leaked a configured value: {body}"
        );
    }
}

/// `explain` is the endpoint that would leak if anything did: in the library
/// it deliberately carries values. Over a socket it carries none of them —
/// not for the path called `password`, and not for the ones nobody called
/// anything.
#[tokio::test]
async fn explain_redacts_every_path_and_not_only_the_ones_that_look_secret() {
    let fixture = fixture();

    for (path, planted) in [("password", SECRET), ("host", "db.internal")] {
        let (status, body) = fixture
            .json(
                &format!("/billing/prod/explain/{path}"),
                Some(common::BILLING_TOKEN),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.to_string().contains(planted),
            "explain served `{path}`'s value: {body}"
        );

        let rows = body["rows"].as_array().expect("explain returns rows");
        let supplying: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|row| row["value"] != serde_json::Value::Null)
            .collect();

        assert!(!supplying.is_empty(), "a configured path has a winner");

        for row in supplying {
            assert_eq!(
                row["value"], "***",
                "a supplying layer must render as `***` and nothing else: {row}"
            );
        }
    }
}

/// The one place a value would travel furthest, and the one this crate can
/// make impossible rather than careful: an `AuditEntry` has no field a value
/// could occupy.
#[tokio::test]
async fn no_value_and_no_token_ever_reaches_the_audit_log() {
    let fixture = fixture();

    // Everything a caller can do: served, denied, unauthenticated, malformed.
    for (uri, token) in [
        ("/billing/prod", Some(common::BILLING_TOKEN)),
        ("/billing/prod/paths", Some(common::BILLING_TOKEN)),
        ("/billing/prod/check", Some(common::BILLING_TOKEN)),
        ("/billing/prod/status", Some(common::BILLING_TOKEN)),
        (
            "/billing/prod/explain/password",
            Some(common::BILLING_TOKEN),
        ),
        ("/billing/staging", Some(common::BILLING_TOKEN)),
        ("/payroll/prod", Some(common::BILLING_TOKEN)),
        ("/ledger/prod", Some(common::BILLING_TOKEN)),
        ("/billing/prod", None),
        ("/billing/prod", Some("wrong-token-0123456789abcdefghijk")),
        ("/billing%0Aadmin/prod", Some(common::BILLING_TOKEN)),
        (
            "/billing/prod/explain/pool..max_size",
            Some(common::BILLING_TOKEN),
        ),
    ] {
        fixture.get(uri, token).await;
    }

    let log = fixture.joined_log();

    assert!(!log.is_empty(), "the fixture audits something");
    for forbidden in [
        SECRET,
        STAGING_SECRET,
        "db.internal",
        "db.staging",
        common::BILLING_TOKEN,
        "wrong-token",
    ] {
        assert!(
            !log.contains(forbidden),
            "`{forbidden}` reached the audit log:\n{log}"
        );
    }
}

/// A path segment is attacker-controlled text and a log line is
/// newline-delimited. The shape check runs before anything is recorded, so
/// there is nothing to escape.
#[tokio::test]
async fn a_request_cannot_forge_an_audit_line() {
    let fixture = fixture();

    fixture
        .get(
            "/billing%0Aaudit%20caller=root%20outcome=served/prod",
            Some(common::BILLING_TOKEN),
        )
        .await;

    let lines = fixture.log_lines();

    assert_eq!(lines.len(), 1, "one request, one line: {lines:?}");
    assert_eq!(
        lines[0],
        "audit caller=billing-pod application=- profile=- endpoint=document \
         outcome=malformed generation=-",
        "a refused shape is logged without the shape"
    );
}

/// The audit log's job: who read what. It records the caller by its
/// *configured* name, which is not a credential.
#[tokio::test]
async fn the_audit_log_records_who_read_which_section_and_which_generation() {
    let fixture = fixture();

    fixture
        .get("/billing/prod", Some(common::BILLING_TOKEN))
        .await;
    fixture
        .get("/payroll/prod", Some(common::BILLING_TOKEN))
        .await;
    fixture.get("/billing/prod", None).await;

    let lines = fixture.log_lines();

    assert_eq!(
        lines,
        [
            "audit caller=billing-pod application=billing profile=prod endpoint=document \
             outcome=served generation=1",
            "audit caller=billing-pod application=payroll profile=prod endpoint=document \
             outcome=not-found generation=-",
            "audit caller=- application=- profile=- endpoint=document \
             outcome=unauthenticated generation=-",
        ]
    );
}

/// A refusal reaches an operator's terminal and a startup log. None of them
/// may carry the token that was refused.
#[tokio::test]
async fn a_startup_refusal_never_prints_a_token() {
    let planted = "planted-startup-token-0123456789";
    let directory = tempfile::tempdir().expect("a temporary directory");
    let billing = directory.path().join("billing.toml");
    std::fs::write(&billing, "[billing]\nhost = 'db'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![section("billing", "prod", billing.display().to_string())],
        // Two clients, one token: a refusal that has to name the problem
        // without naming the credential.
        clients: vec![
            client("one", planted, &["billing"]),
            client("two", planted, &["billing"]),
        ],
        ..ServerConfig::default()
    };

    let error = dynamic_config_server::Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("two clients share a token");

    let rendered = format!("{error} / {error:?}");

    assert!(
        !rendered.contains(planted),
        "a credential escaped through a startup refusal: {rendered}"
    );
    assert!(rendered.contains("token"), "{rendered}");
}

/// A section whose sources will not load is fatal, and the message names the
/// section rather than what was in the file.
#[tokio::test]
async fn a_section_that_will_not_load_refuses_to_start_without_quoting_the_file() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let billing = directory.path().join("billing.toml");
    std::fs::write(
        &billing,
        "[billing]\npassword = 'hunter2'\nthis is not toml",
    )
    .expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![section("billing", "prod", billing.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let error = dynamic_config_server::Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("the file does not parse");
    let rendered = error.to_string();

    assert!(rendered.contains("billing"), "{rendered}");
    assert!(
        !rendered.contains(SECRET),
        "a startup error quoted the file: {rendered}"
    );
}

/// And the whole thing once more with a server the fixture did not build, so
/// nothing above rests on the fixture's own shape.
#[tokio::test]
async fn a_single_section_server_keeps_the_same_boundary() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("only.toml");
    std::fs::write(
        &file,
        "[only]\ntoken = 'planted-response-value'\n[only.nested]\ndeep = 'also-planted'\n",
    )
    .expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        sections: vec![section("only", "dev", file.display().to_string())],
        clients: vec![client("reader", common::BILLING_TOKEN, &["only"])],
        ..ServerConfig::default()
    };
    let fixture = start(directory, config);

    let (_, document) = fixture.get("/only/dev", Some(common::BILLING_TOKEN)).await;
    assert!(document.contains("planted-response-value"));
    assert!(document.contains("also-planted"));

    for uri in [
        "/only/dev/paths",
        "/only/dev/check",
        "/only/dev/status",
        "/only/dev/explain/token",
        "/only/dev/explain/nested.deep",
    ] {
        let (status, body) = fixture.get(uri, Some(common::BILLING_TOKEN)).await;

        assert_eq!(status, StatusCode::OK, "`{uri}`");
        assert!(!body.contains("planted-response-value"), "`{uri}`: {body}");
        assert!(!body.contains("also-planted"), "`{uri}`: {body}");
    }

    let log = fixture.joined_log();
    assert!(!log.contains("planted-response-value"), "{log}");
    assert!(!log.contains("also-planted"), "{log}");
}

/// **The sharpest secret in this program's whole surface.**
///
/// A configured token is a credential; a private key is a credential that
/// also authenticates the server to everybody else, and it is the one file
/// here whose bytes have no legitimate destination at all — not a response,
/// not a log, not a `Debug`. So this asserts more than the token tests do:
/// every error a key file can produce, plus the `Debug` of everything that
/// holds one, and the key's own base64 must appear in none of them.
#[cfg(feature = "tls")]
#[tokio::test]
async fn a_private_key_never_reaches_a_diagnostic() {
    use common::certificates::Authority;
    use dynamic_config_server::TlsConfig;

    let directory = tempfile::tempdir().expect("a temporary directory");
    let authority = Authority::new(directory.path(), "configured");
    let good = authority.sign(directory.path(), "server", true, 0o600);
    let other = authority.sign(directory.path(), "other", true, 0o600);
    let exposed = authority.sign(directory.path(), "exposed", true, 0o644);
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nhost = 'db'\n").expect("writable");

    // The middle of the PEM body: distinctive enough that finding it
    // anywhere is unambiguous, and present in no armour line.
    let planted: Vec<String> = [&good.key, &other.key, &exposed.key]
        .iter()
        .map(|path| {
            let pem = std::fs::read_to_string(path).expect("the key was written");
            let body: Vec<&str> = pem
                .lines()
                .filter(|line| !line.starts_with("-----"))
                .collect();

            assert!(!body.is_empty(), "a key has a body");
            body.join("")
        })
        .collect();

    let configuration = |certificate: &std::path::Path, key: &std::path::Path| ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        tls: Some(TlsConfig {
            certificate: certificate.display().to_string(),
            key: key.display().to_string(),
            client_ca: None,
            crl: None,
        }),
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let unparsable = directory.path().join("garbage.key");
    std::fs::write(
        &unparsable,
        format!("-----BEGIN PRIVATE KEY-----\n{}\n", planted[0]),
    )
    .expect("writable");

    let mut rendered = Vec::new();

    for (certificate, key) in [
        // Every way a key file can be wrong: absent, world-readable,
        // unparsable, and the key of a different certificate.
        (&good.certificate, &directory.path().join("absent.key")),
        (&exposed.certificate, &exposed.key),
        (&good.certificate, &unparsable),
        (&good.certificate, &other.key),
    ] {
        let error = dynamic_config_server::Server::start_with(
            &configuration(certificate, key),
            dynamic_config_server::NoAudit,
        )
        .expect_err("every one of these is a refusal");

        rendered.push(format!("{error} / {error:?}"));
    }

    // And the server that *did* start, which is holding a usable key for as
    // long as it runs.
    let server = dynamic_config_server::Server::start_with(
        &configuration(&good.certificate, &good.key),
        dynamic_config_server::NoAudit,
    )
    .expect("a matching certificate and key start");

    rendered.push(format!("{server:?}"));
    rendered.push(format!("{:?}", server.tls().expect("TLS is configured")));
    rendered.push(server.posture().to_owned());

    for text in &rendered {
        for key in &planted {
            assert!(
                !text.contains(key.as_str()),
                "a private key escaped through a diagnostic: {text}"
            );
        }
        assert!(
            !text.contains("PRIVATE KEY"),
            "not even the armour, which is one grep away from the rest: {text}"
        );
    }
}

/// A metric label is the least guarded diagnostic a server has — a scrape
/// leaves the process on a schedule, unasked — so the boundary there is
/// drawn tighter than anywhere else: not the value, and not the key path
/// or the file either, both of which `/status` is allowed to return to an
/// authorised caller and neither of which a label may carry.
#[tokio::test]
async fn a_scrape_carries_no_value_no_key_path_and_no_file_name() {
    let fixture = fixture();

    fixture.write(
        "billing.toml",
        &format!("[billing]\nhost = 'db.internal'\npassword = '{SECRET}'\nthis is not toml"),
    );
    fixture
        .server
        .section("billing", "prod")
        .expect("the fixture serves it")
        .reload()
        .expect_err("the file no longer parses");

    let (status, body) = fixture.get("/metrics", Some(common::BILLING_TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(SECRET), "a value reached a label: {body}");
    assert!(
        !body.contains("db.internal"),
        "no value, secret or not: {body}"
    );
    assert!(
        !body.contains("password") && !body.contains("host"),
        "a key path is unbounded cardinality as well as a disclosure: {body}"
    );
    assert!(
        !body.contains("billing.toml"),
        "and a file name is both too: {body}"
    );
    assert!(
        body.contains(r#"kind="parse""#),
        "the category is what a label may carry: {body}"
    );
}

/// The change stream is not a second door for values.
///
/// It is the endpoint where one would be easiest to open by accident: a
/// body that outlives the request that authorised it, written to on every
/// install, in a place a browser can read. So what goes down it is a
/// generation number and the two names the caller already supplied — and
/// this asserts it across an install that changes a secret, which is
/// exactly the moment a diff-carrying design would emit one.
#[tokio::test]
async fn a_change_stream_carries_no_value_and_no_key_path() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    let fixture = fixture();

    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/billing/prod/stream")
                .header("authorization", format!("Bearer {}", common::BILLING_TOKEN))
                .body(Body::empty())
                .expect("a well-formed request"),
        )
        .await
        .expect("the router is infallible");

    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();

    async fn next(body: &mut Body) -> String {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("an event arrives promptly")
            .expect("the stream has not ended")
            .expect("a frame rather than an error");

        String::from_utf8(frame.into_data().expect("a data frame").to_vec())
            .expect("every event is text")
    }

    let mut events = next(&mut body).await;

    // The install that would tempt a design into carrying a diff: a secret
    // changes, and the only thing that may travel is that something did.
    fixture.write(
        "billing.toml",
        "[billing]\nhost = 'db.internal'\npassword = 'hunter2-rotated'\n\n[billing.pool]\nmax_size = 9\n",
    );
    fixture
        .server
        .section("billing", "prod")
        .expect("the fixture serves it")
        .reload()
        .expect("the rewritten file loads");

    events.push_str(&next(&mut body).await);

    assert!(
        !events.contains(SECRET),
        "a value reached the stream: {events}"
    );
    assert!(
        !events.contains("hunter2-rotated"),
        "including the one that just changed: {events}"
    );
    assert!(
        !events.contains("password") && !events.contains("max_size"),
        "a changed key path is not carried either: {events}"
    );
    assert!(
        events.contains(r#""generation":2"#),
        "what travels is the number: {events}"
    );
}
