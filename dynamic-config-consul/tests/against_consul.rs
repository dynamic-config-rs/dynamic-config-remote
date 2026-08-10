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
use testcontainers::runners::SyncRunner;
use testcontainers_modules::consul::Consul as ConsulImage;

struct Running {
    address: String,
    _container: testcontainers::Container<ConsulImage>,
}

/// Starts Consul and writes one key into it.
fn consul_with(key: &str, value: &str) -> Running {
    let container = ConsulImage::default()
        .start()
        .expect("Docker should be available; these tests exercise a real Consul");

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

    let container = ConsulImage::default()
        .with_env_var(
            "CONSUL_LOCAL_CONFIG",
            r#"{"acl":{"enabled":true,"default_policy":"deny","tokens":{"initial_management":"root-token"}}}"#,
        )
        .start()
        .expect("Docker should be available");

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

    assert_eq!(refused.kind(), dynamic_config::ErrorKind::Remote);

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
