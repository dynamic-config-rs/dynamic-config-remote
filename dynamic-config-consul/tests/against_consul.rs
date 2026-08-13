//! Against a real Consul, in a container.
//!
//! ```text
//! cargo test -p dynamic-config-consul
//! ```
//!
//! Needs a working Docker daemon. Without one these fail rather than skipping:
//! a test that quietly stops running is one nobody notices has stopped.

use dynamic_config::{Format, RemoteSource};
use dynamic_config_consul::Consul;
use testcontainers_modules::consul::Consul as ConsulImage;

struct Running {
    address: String,
    _container: testcontainers::Container<ConsulImage>,
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

/// Starts Consul and writes one key into it.
fn consul_with(key: &str, value: &str) -> Running {
    let container = start_resilient(ConsulImage::default);

    let port = container
        .get_host_port_ipv4(8500)
        .expect("Consul should expose its HTTP port");
    let address = format!("http://127.0.0.1:{port}");

    let response = ureq::put(&format!("{address}/v1/kv/{key}"))
        .send(value)
        .expect("writing the key should succeed");

    assert!(
        response.status().is_success(),
        "unexpected status {}",
        response.status()
    );

    Running {
        address,
        _container: container,
    }
}

#[test]
fn a_key_holds_a_whole_configuration_document() {
    let consul = consul_with(
        "myapp/db.json",
        r#"{"db": {"host": "localhost", "port": 5432}}"#,
    );

    let source = Consul::new(&consul.address, "myapp/db.json");
    let fetched = source.fetch().expect("the key is there");

    assert_eq!(
        fetched.format,
        Format::Json,
        "the format comes from the key's extension"
    );
    assert!(fetched.text.contains("localhost"), "{}", fetched.text);
}

#[test]
fn the_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
    }

    let consul = consul_with(
        "myapp/loads.json",
        r#"{"db": {"host": "db.internal", "port": 6432}}"#,
    );

    let fetched = Consul::new(&consul.address, "myapp/loads.json")
        .fetch()
        .unwrap();

    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];
    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("the key holds a whole document");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
        }
    );
}

#[test]
fn a_key_with_no_extension_needs_the_format_stating() {
    let consul = consul_with("myapp/plain", r#"{"db": {"host": "x"}}"#);

    let error = Consul::new(&consul.address, "myapp/plain")
        .fetch()
        .expect_err("nothing says what format this is");

    assert!(error.to_string().contains("with_format"), "{error}");

    // Stated, it reads fine.
    let fetched = Consul::new(&consul.address, "myapp/plain")
        .with_format(Format::Json)
        .fetch()
        .expect("the format is stated now");

    assert!(fetched.text.contains("\"x\""), "{}", fetched.text);
}

#[test]
fn a_key_that_is_not_there_is_a_remote_error() {
    let consul = consul_with("myapp/present.json", r#"{"db": {}}"#);

    let error = Consul::new(&consul.address, "myapp/absent.json")
        .fetch()
        .expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    // `describe()` names the agent's address and the key, so a program with
    // two Consuls never has to guess which one answered.
    assert!(
        error.to_string().contains("kv/myapp/absent.json"),
        "{error}"
    );
    assert!(error.to_string().contains(&consul.address), "{error}");
}

// ---------------------------------------------------------------------------
// Several keys as one document
// ---------------------------------------------------------------------------

use dynamic_config::Value;
use dynamic_config_consul::Keys;

/// Starts Consul and writes every `(key, value)` pair into it.
fn consul_holding(pairs: &[(&str, &str)]) -> Running {
    let running = consul_with(pairs[0].0, pairs[0].1);

    for (key, value) in &pairs[1..] {
        let response = ureq::put(&format!("{}/v1/kv/{key}", running.address))
            .send(*value)
            .expect("writing the key should succeed");

        assert!(response.status().is_success(), "{}", response.status());
    }

    running
}

/// The rule a named list inherits from a list of `.file(..)` calls: call
/// order, and the later key wins.
#[test]
fn named_keys_merge_in_call_order_and_the_later_one_wins() {
    let consul = consul_holding(&[
        (
            "myapp/base.json",
            r#"{"db": {"host": "base", "port": 5432}}"#,
        ),
        ("myapp/local.json", r#"{"db": {"port": 6432}}"#),
    ]);

    let source = Consul::new(
        &consul.address,
        Keys::several(["myapp/base.json", "myapp/local.json"]),
    );

    let fetched = source.fetch().expect("both keys are there");
    let merged = Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("base".to_owned())),
        "a key the later document never mentions survives"
    );
    assert_eq!(
        merged.get("db.port"),
        Some(&Value::Integer(6432)),
        "and the later document wins where they meet"
    );
}

/// The other half, through Consul's own `?recurse`: one request, the whole
/// subtree, folded into one document.
#[test]
fn a_prefix_folds_its_sections_into_one_document() {
    let consul = consul_holding(&[
        ("myapp/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("myapp/server.json", r#"{"server": {"port": 8080}}"#),
        // Outside the prefix: proof the recursion is bounded where it says.
        ("other/db.json", r#"{"db": {"host": "wrong"}}"#),
    ]);

    let source = Consul::new(&consul.address, Keys::prefix("myapp/")).with_format(Format::Json);

    let fetched = source.fetch().expect("the subtree is readable");
    let merged = Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("db.internal".to_owned()))
    );
    assert_eq!(merged.get("server.port"), Some(&Value::Integer(8080)));
}

/// A prefix says "these are disjoint sections". Two of them supplying one path
/// is a deployment bug, named rather than resolved — and named by path, never
/// by value.
#[test]
fn two_keys_under_a_prefix_supplying_one_path_are_refused_by_name() {
    let consul = consul_holding(&[
        ("clash/db.json", r#"{"db": {"password": "hunter2-first"}}"#),
        (
            "clash/extra.json",
            r#"{"db": {"password": "hunter2-second"}}"#,
        ),
    ]);

    let source = Consul::new(&consul.address, Keys::prefix("clash/")).with_format(Format::Json);

    let error = source.fetch().expect_err("both keys supply db.password");
    let printed = format!("{error} {error:?}");

    assert!(printed.contains("clash/db.json"), "{printed}");
    assert!(printed.contains("clash/extra.json"), "{printed}");
    assert!(printed.contains("db.password"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a collision report names paths and never values: {printed}"
    );
}

/// Fail-whole, not merge-what-came-back: a configuration quietly missing a
/// section is worse than a refresh that failed and left the last one serving.
#[test]
fn one_unreadable_key_fails_the_whole_fetch_and_names_it() {
    let consul = consul_holding(&[("partial/db.json", r#"{"db": {"host": "here"}}"#)]);

    let source = Consul::new(
        &consul.address,
        Keys::several(["partial/db.json", "partial/absent.json"]),
    );

    let error = source.fetch().expect_err("one of the two is not there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("partial/absent.json"), "{error}");
}

/// The end of the feature: a merged document is a document, and loads like one.
#[test]
fn a_merged_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
    }

    let consul = consul_holding(&[
        ("split/host.json", r#"{"db": {"host": "db.internal"}}"#),
        ("split/port.json", r#"{"db": {"port": 6432}}"#),
    ]);

    let source = Consul::new(&consul.address, Keys::prefix("split/")).with_format(Format::Json);
    let fetched = source.fetch().unwrap();

    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];
    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("two keys became one document");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
        }
    );

    // Provenance is store-grained from here on: one layer, so `source_of`
    // answers with the store and the set it read.
    assert!(
        source.describe().contains("prefix split/"),
        "{}",
        source.describe()
    );
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

use std::sync::mpsc;
use std::time::Duration;

use dynamic_config::RemoteWatch;

/// Writes a key, returning whether Consul accepted it.
fn put(address: &str, key: &str, value: &str) {
    let response = ureq::put(&format!("{address}/v1/kv/{key}"))
        .send(value)
        .expect("writing the key should succeed");

    assert!(response.status().is_success(), "{}", response.status());
}

/// A blocking query returns the moment the value moves — not on a timer.
///
/// The wait is set well above the assertion's deadline on purpose: if this
/// passed because the query timed out and looped, rather than because Consul
/// answered, it would take ten seconds rather than one.
#[test]
fn a_change_arrives_because_it_changed_not_because_time_passed() {
    let consul = consul_with("myapp/watched.json", r#"{"db": {"host": "first"}}"#);

    let source =
        Consul::new(&consul.address, "myapp/watched.json").with_wait(Duration::from_secs(10));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // The first query carries index 0 and returns at once, so the loop is
    // parked on a real blocking query within a moment.
    std::thread::sleep(Duration::from_millis(500));

    let written = std::time::Instant::now();

    put(
        &consul.address,
        "myapp/watched.json",
        r#"{"db": {"host": "second"}}"#,
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the blocking query should return when the key moves");

    assert!(
        written.elapsed() < Duration::from_secs(5),
        "delivery took {:?}, which looks like a timeout rather than a change",
        written.elapsed()
    );
    assert_eq!(document.format, Format::Json);
    assert!(document.text.contains("second"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// The value present at startup is not a change, so it is not reported.
#[test]
fn the_starting_value_is_not_announced() {
    let consul = consul_with("myapp/quiet.json", r#"{"db": {"host": "first"}}"#);

    let source = Consul::new(&consul.address, "myapp/quiet.json").with_wait(Duration::from_secs(2));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    assert!(
        receiver.recv_timeout(Duration::from_secs(3)).is_err(),
        "a watch reports changes; announcing the current value would make every \
         restart look like an edit"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// The same write twice is one change, not two: Consul bumps its index on every
/// write, including one that changed nothing.
#[test]
fn an_identical_write_is_not_reported_twice() {
    let consul = consul_with("myapp/same.json", r#"{"db": {"host": "first"}}"#);

    let source = Consul::new(&consul.address, "myapp/same.json").with_wait(Duration::from_secs(2));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // Let the priming query settle first. Writing into that race would prime
    // the loop with the *new* value, and then the change under test never
    // happens — which is a test that hangs, not one that fails.
    std::thread::sleep(Duration::from_millis(500));

    put(
        &consul.address,
        "myapp/same.json",
        r#"{"db": {"host": "second"}}"#,
    );

    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the first change is reported");

    // Written again, byte for byte.
    put(
        &consul.address,
        "myapp/same.json",
        r#"{"db": {"host": "second"}}"#,
    );

    assert!(
        receiver.recv_timeout(Duration::from_secs(3)).is_err(),
        "an unchanged document must not be pushed through as a reload"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// Stopping a loop parked in a blocking query is bounded by the wait, and this
/// pins that it is bounded at all rather than hanging forever.
#[test]
fn stopping_ends_the_loop() {
    let consul = consul_with("myapp/stopped.json", r#"{"db": {"host": "first"}}"#);

    // A short wait, because this test measures how long stopping takes.
    let source =
        Consul::new(&consul.address, "myapp/stopped.json").with_wait(Duration::from_secs(1));

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || source.watch(&watching, |_| Ok(())));

    std::thread::sleep(Duration::from_millis(200));
    watch.stop();

    let started = std::time::Instant::now();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");

    assert!(
        started.elapsed() < Duration::from_secs(15),
        "stopping took {:?}, which is not bounded by the wait",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

use dynamic_config_consul::Auth;

/// A Consul started with ACLs on, so a token is actually required.
fn secured_consul(key: &str, value: &str) -> Running {
    use testcontainers::ImageExt;

    let container = start_resilient(|| {
        ConsulImage::default().with_env_var(
            "CONSUL_LOCAL_CONFIG",
            r#"{"acl":{"enabled":true,"default_policy":"deny","tokens":{"initial_management":"root-token"}}}"#,
        )
    });

    let port = container
        .get_host_port_ipv4(8500)
        .expect("Consul should expose its HTTP port");
    let address = format!("http://127.0.0.1:{port}");

    // The ACL system takes a moment to come up after the agent does.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    loop {
        let wrote = ureq::put(&format!("{address}/v1/kv/{key}"))
            .header("X-Consul-Token", "root-token")
            .send(value);

        match wrote {
            Ok(response) if response.status().is_success() => break,
            _ if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            other => panic!("ACLs never came up: {other:?}"),
        }
    }

    Running {
        address,
        _container: container,
    }
}

#[test]
fn a_management_token_reads_what_the_anonymous_one_cannot() {
    let consul = secured_consul("myapp/secured.json", r#"{"db": {"host": "secured"}}"#);

    let refused = Consul::new(&consul.address, "myapp/secured.json")
        .fetch()
        .expect_err("default_policy is deny");

    // `Auth`, not `Remote`: the agent answered, and what it said was no. A
    // caller can stop rather than back off, because waiting will not grant
    // the anonymous token a policy it does not have.
    assert_eq!(refused.kind(), dynamic_config::ErrorKind::Auth);

    let allowed = Consul::new(&consul.address, "myapp/secured.json")
        .with_token("root-token")
        .fetch()
        .expect("a management token reads anything");

    assert!(allowed.text.contains("secured"), "{}", allowed.text);
}

#[test]
fn the_token_from_the_environment_is_used() {
    let consul = secured_consul("myapp/from-env.json", r#"{"db": {"host": "from-env"}}"#);

    // Set and removed around one `Auth::from_environment()` call, not around the
    // read: the variable is read once, when the auth is built.
    std::env::set_var("CONSUL_HTTP_TOKEN", "root-token");
    let auth = Auth::from_environment();
    std::env::remove_var("CONSUL_HTTP_TOKEN");

    let fetched = Consul::new(&consul.address, "myapp/from-env.json")
        .with_auth(auth)
        .fetch()
        .expect("the environment supplied a management token");

    assert!(fetched.text.contains("from-env"), "{}", fetched.text);
}

#[test]
fn a_login_against_a_method_that_does_not_exist_names_it() {
    let consul = secured_consul("myapp/nomethod.json", r#"{"db": {"host": "x"}}"#);

    let error = Consul::new(&consul.address, "myapp/nomethod.json")
        .with_auth(Auth::jwt("kubernetes", "a.b.c"))
        .fetch()
        .expect_err("no auth method is configured");

    assert!(error.to_string().contains("kubernetes"), "{error}");
}

/// An agent the caller already has — their proxy settings, their CA, their pool.
#[test]
fn an_existing_agent_is_used_as_it_is() {
    let consul = consul_with("myapp/agent.json", r#"{"db": {"host": "via-agent"}}"#);

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();

    let fetched = Consul::new(&consul.address, "myapp/agent.json")
        .with_agent(agent)
        .fetch()
        .expect("the supplied agent should be used");

    assert!(fetched.text.contains("via-agent"), "{}", fetched.text);
}

/// A deleted key is not a configuration change: the running snapshot stays,
/// and the watch survives for the key's return.
#[test]
fn a_deleted_key_is_not_reported_as_a_change() {
    let consul = consul_with("myapp/doomed.json", r#"{"db": {"host": "first"}}"#);

    let source =
        Consul::new(&consul.address, "myapp/doomed.json").with_wait(Duration::from_secs(10));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // Let the priming query pass, then delete the key.
    std::thread::sleep(Duration::from_millis(500));

    let response = ureq::delete(&format!("{}/v1/kv/myapp/doomed.json", consul.address))
        .call()
        .expect("deleting the key should succeed");
    assert!(response.status().is_success());

    let quiet = receiver.recv_timeout(Duration::from_secs(2));
    assert!(
        quiet.is_err(),
        "no configuration is not a configuration; a deletion must not reach the callback"
    );

    // The key coming back IS a change, which also proves the loop survived.
    put(
        &consul.address,
        "myapp/doomed.json",
        r#"{"db": {"host": "back"}}"#,
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the key's return should be noticed");
    assert!(document.text.contains("back"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

// ---------------------------------------------------------------------------
// Watching a subtree
// ---------------------------------------------------------------------------

use base64::Engine;

/// Writes both halves of the set at one index, through Consul's transaction
/// endpoint.
///
/// Atomic on purpose: it takes a torn *write* off the table, so a delivery
/// whose halves disagree can only be a torn *read* — which is the thing under
/// test.
fn stamp_together(address: &str, generation: u64) {
    let operations: Vec<String> = [
        (
            "stamped/db.json",
            format!(r#"{{"db": {{"generation": {generation}}}}}"#),
        ),
        (
            "stamped/server.json",
            format!(r#"{{"server": {{"generation": {generation}}}}}"#),
        ),
    ]
    .iter()
    .map(|(key, document)| {
        let encoded = base64::engine::general_purpose::STANDARD.encode(document);

        format!(r#"{{"KV": {{"Verb": "set", "Key": "{key}", "Value": "{encoded}"}}}}"#)
    })
    .collect();

    let response = ureq::put(&format!("{address}/v1/txn"))
        .send(format!("[{}]", operations.join(",")))
        .expect("the transaction should be accepted");

    assert!(response.status().is_success(), "{}", response.status());
}

/// The property a watch on a set exists to have: **every delivery agrees with
/// itself**.
///
/// One generation is stamped into both sections of the subtree, in one Consul
/// transaction, so every index the agent ever holds has the two halves equal. A
/// watch that woke on the subtree and then read it back key by key — or read it
/// back at all, with a second transaction landing in between — would eventually
/// deliver `db.generation` from one write beside `server.generation` from
/// another: a document that never existed at any index. A recursive blocking
/// query has no such window, because its own answer is the document, and this
/// is the test that would catch that going away.
#[test]
fn a_prefix_watch_never_delivers_a_document_that_never_existed() {
    let consul = consul_holding(&[
        ("stamped/db.json", r#"{"db": {"generation": 0}}"#),
        ("stamped/server.json", r#"{"server": {"generation": 0}}"#),
    ]);

    let source = Consul::new(&consul.address, Keys::prefix("stamped/"))
        .with_format(Format::Json)
        .with_wait(Duration::from_secs(10));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document.text);
            Ok(())
        })
    });

    // The first query carries index 0 and returns at once, so the loop is
    // parked on a real blocking query within a moment.
    std::thread::sleep(Duration::from_millis(500));

    for generation in 1..=6 {
        stamp_together(&consul.address, generation);

        // Enough for the parked query to return and the next one to be
        // issued; a run that coalesces two writes is fine, and is why the
        // assertion below is on the highest generation rather than a count.
        std::thread::sleep(Duration::from_millis(150));
    }

    let mut seen = 0;
    let mut highest = 0;

    while let Ok(text) = receiver.recv_timeout(Duration::from_secs(10)) {
        let tree = Value::parse(&text, Format::Json).expect("every delivery is a document");

        let db = tree.get("db.generation").cloned();
        let server = tree.get("server.generation").cloned();

        assert_eq!(
            db, server,
            "a delivery whose two halves disagree is a document that never \
             existed at any index: {text}"
        );

        if let Some(Value::Integer(generation)) = db {
            highest = highest.max(generation);
        }

        seen += 1;

        if highest == 6 {
            break;
        }
    }

    assert!(seen > 0, "the watcher must have delivered something");
    assert_eq!(highest, 6, "and must have caught up to the last write");

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// A key leaving the subtree changes the set, so the document that follows is
/// simply the sections that are left — the same answer `fetch` would give.
#[test]
fn a_deletion_under_a_watched_prefix_is_a_change_to_the_set() {
    let consul = consul_holding(&[
        ("shrinking/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("shrinking/server.json", r#"{"server": {"port": 8080}}"#),
        ("shrinking/extra.json", r#"{"extra": {"on": true}}"#),
    ]);

    let source = Consul::new(&consul.address, Keys::prefix("shrinking/"))
        .with_format(Format::Json)
        .with_wait(Duration::from_secs(10));

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document.text);
            Ok(())
        })
    });

    std::thread::sleep(Duration::from_millis(500));

    let response = ureq::delete(&format!("{}/v1/kv/shrinking/extra.json", consul.address))
        .call()
        .expect("deleting the key should succeed");
    assert!(response.status().is_success());

    let text = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("a key leaving the subtree is a change to the set");

    let tree = Value::parse(&text, Format::Json).expect("it is a document");

    assert_eq!(tree.get("extra.on"), None, "the deleted section is gone");
    assert_eq!(
        tree.get("server.port"),
        Some(&Value::Integer(8080)),
        "and the sections that remain are all there: {text}"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}
