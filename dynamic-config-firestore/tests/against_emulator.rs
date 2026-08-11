//! Against the real Firestore emulator, in a container.
//!
//! Google ships the emulator inside the Cloud SDK image, so this drives that
//! rather than a mock: what is being checked is Firestore's value encoding and
//! its REST surface, and a mock of those would only confirm what we already
//! believed about them.
//!
//! ```text
//! cargo test -p dynamic-config-firestore
//! ```

use std::time::Duration;

use dynamic_config::{Format, RemoteSource, RemoteWatch};
use dynamic_config_firestore::Firestore;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

const PROJECT: &str = "test-project";

struct Running {
    endpoint: String,
    _container: testcontainers::Container<GenericImage>,
}

/// `start()`, retried once with a fresh container.
///
/// On a busy shared runner the first boot occasionally loses the scheduling
/// lottery — `WaitContainer(StartupTimeout)` from a daemon that was going to
/// be fine in ten more seconds. One fresh attempt separates a slow neighbour
/// from an actual failure; failing twice is behaviour, and panics with both
/// errors.
fn start_resilient<I, R>(make: impl Fn() -> R) -> testcontainers::Container<I>
where
    I: testcontainers::Image,
    R: testcontainers::runners::SyncRunner<I>,
{
    match make().start() {
        Ok(container) => container,
        Err(first) => {
            eprintln!("container start failed ({first}); retrying once with a fresh container");
            // Not immediately: the retry that follows a lost scheduling
            // lottery without pausing is the attempt most likely to lose
            // the same one.
            std::thread::sleep(std::time::Duration::from_secs(2));
            make().start().unwrap_or_else(|second| {
                panic!(
                    "the container failed to start twice; is Docker available? \
                     first: {first}; then: {second}"
                )
            })
        }
    }
}

fn emulator() -> Running {
    // Google's own registry rather than Docker Hub: no anonymous pull limits,
    // which a test suite hits long before a person does.
    let container = start_resilient(|| {
        GenericImage::new(
            "gcr.io/google.com/cloudsdktool/google-cloud-cli",
            "emulators",
        )
        .with_exposed_port(8080.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Dev App Server is now running"))
        .with_cmd([
            "gcloud",
            "emulators",
            "firestore",
            "start",
            "--host-port=0.0.0.0:8080",
        ])
    });

    let port = container
        .get_host_port_ipv4(8080)
        .expect("the emulator should expose its port");

    Running {
        endpoint: format!("http://127.0.0.1:{port}"),
        _container: container,
    }
}

/// Writes a document through the emulator's own REST API.
fn write(endpoint: &str, path: &str, fields: serde_json::Value) {
    let (collection, document) = path.rsplit_once('/').expect("collection/document");

    let url = format!(
        "{endpoint}/v1/projects/{PROJECT}/databases/(default)/documents/{collection}?documentId={document}"
    );

    // `POST` creates; if it is already there, `PATCH` replaces it.
    let created = ureq::post(&url).send_json(serde_json::json!({ "fields": fields.clone() }));

    if created.is_err() {
        let patch = format!(
            "{endpoint}/v1/projects/{PROJECT}/databases/(default)/documents/{collection}/{document}"
        );

        ureq::patch(&patch)
            .send_json(serde_json::json!({ "fields": fields }))
            .expect("replacing the document should succeed");
    }
}

fn source(endpoint: &str, path: &str) -> Firestore {
    Firestore::new(PROJECT, path).with_endpoint(endpoint)
}

#[test]
fn a_document_becomes_a_configuration_section() {
    let firestore = emulator();

    write(
        &firestore.endpoint,
        "config/db",
        serde_json::json!({
            "host": { "stringValue": "localhost" },
            "port": { "integerValue": "5432" },
        }),
    );

    let fetched = source(&firestore.endpoint, "config/db")
        .fetch()
        .expect("the document is there");

    assert_eq!(fetched.format, Format::Json);
    assert!(fetched.text.contains("localhost"), "{}", fetched.text);
    // The integer arrived as a string and must not still be one.
    assert!(fetched.text.contains("5432"), "{}", fetched.text);
    assert!(!fetched.text.contains("\"5432\""), "{}", fetched.text);
}

#[test]
fn the_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
        pool: Pool,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Pool {
        max_size: u16,
    }

    let firestore = emulator();

    write(
        &firestore.endpoint,
        "config/loads",
        serde_json::json!({
            "host": { "stringValue": "db.internal" },
            "port": { "integerValue": "6432" },
            "pool": {
                "mapValue": { "fields": { "max_size": { "integerValue": "10" } } }
            },
        }),
    );

    let fetched = source(&firestore.endpoint, "config/loads").fetch().unwrap();
    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];

    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("the document is a whole section");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
            pool: Pool { max_size: 10 },
        }
    );
}

#[test]
fn a_document_that_is_not_there_is_a_remote_error() {
    let firestore = emulator();

    let error = source(&firestore.endpoint, "config/absent")
        .fetch()
        .expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
}

#[test]
fn a_change_reaches_the_callback() {
    use std::sync::mpsc;

    let firestore = emulator();

    write(
        &firestore.endpoint,
        "config/watched",
        serde_json::json!({ "host": { "stringValue": "first" } }),
    );

    let watcher = source(&firestore.endpoint, "config/watched");
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        watcher.watch(&watching, Duration::from_millis(200), move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // The first read records the update time without firing.
    std::thread::sleep(Duration::from_millis(700));

    write(
        &firestore.endpoint,
        "config/watched",
        serde_json::json!({ "host": { "stringValue": "second" } }),
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("a new update time should be noticed");

    assert!(document.text.contains("second"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// A deleted document is not a configuration change: reads start failing,
/// failed checks are skipped, and the watch stays alive for the document's
/// return.
#[test]
fn a_deleted_document_is_not_reported_as_a_change() {
    use std::sync::mpsc;

    let firestore = emulator();

    write(
        &firestore.endpoint,
        "config/doomed",
        serde_json::json!({ "host": { "stringValue": "first" } }),
    );

    let watcher = source(&firestore.endpoint, "config/doomed");
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        watcher.watch(&watching, Duration::from_millis(200), move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // Let the first tick record the update time, then delete the document.
    std::thread::sleep(Duration::from_millis(700));

    let url = format!(
        "{}/v1/projects/{PROJECT}/databases/(default)/documents/config/doomed",
        firestore.endpoint
    );
    let response = ureq::delete(&url)
        .call()
        .expect("deleting the document should succeed");
    assert!(response.status().is_success());

    let quiet = receiver.recv_timeout(Duration::from_secs(2));
    assert!(
        quiet.is_err(),
        "no configuration is not a configuration; a deletion must not reach the callback"
    );

    // The document coming back IS a change, which also proves the loop
    // survived the stretch of failing reads.
    write(
        &firestore.endpoint,
        "config/doomed",
        serde_json::json!({ "host": { "stringValue": "back" } }),
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("the document's return should be noticed");
    assert!(document.text.contains("back"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}
