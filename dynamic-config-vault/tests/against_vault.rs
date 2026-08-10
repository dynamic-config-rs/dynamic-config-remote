//! Against a real Vault, in a container.
//!
//! ```text
//! cargo test -p dynamic-config-vault
//! ```
//!
//! Needs a working Docker daemon. Without one these do not compile out or
//! silently pass — they fail, because a test that quietly skips itself is a
//! test nobody notices has stopped running.

use dynamic_config::{Format, RemoteSource};
use dynamic_config_vault::Vault;
use testcontainers::runners::SyncRunner;
use testcontainers_modules::hashicorp_vault::HashicorpVault;

/// The dev-mode root token `testcontainers-modules` starts Vault with.
const TOKEN: &str = "myroot";

struct Running {
    address: String,
    _container: testcontainers::Container<HashicorpVault>,
}

/// Starts Vault and writes one KV v2 secret into it.
fn vault_with(path: &str, secret: serde_json::Value) -> Running {
    let container = HashicorpVault::default()
        .start()
        .expect("Docker should be available; these tests exercise a real Vault");

    let port = container
        .get_host_port_ipv4(8200)
        .expect("Vault should expose its API port");
    let address = format!("http://127.0.0.1:{port}");

    // `secret/` is mounted as KV v2 in dev mode, so this is a v2 write.
    let response = ureq::post(&format!("{address}/v1/secret/data/{path}"))
        .header("X-Vault-Token", TOKEN)
        .send_json(serde_json::json!({ "data": secret }))
        .expect("writing the secret should succeed");

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
fn a_secret_becomes_a_configuration_document() {
    let vault = vault_with(
        "myapp/db",
        serde_json::json!({ "host": "localhost", "port": 5432 }),
    );

    let source = Vault::new(&vault.address, "secret", "myapp/db")
        .with_key("db")
        .with_token(TOKEN);

    let fetched = source.fetch().expect("the secret is there");

    assert_eq!(fetched.format, Format::Json);

    // Wrapped under the section key, so it merges like any other source.
    let parsed: serde_json::Value = serde_json::from_str(&fetched.text).unwrap();
    assert_eq!(parsed["db"]["host"], "localhost");
    assert_eq!(parsed["db"]["port"], 5432);
}

#[test]
fn the_document_loads_into_a_struct() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Db {
        host: String,
        port: u16,
    }

    let vault = vault_with(
        "myapp/loads",
        serde_json::json!({ "host": "db.internal", "port": 6432 }),
    );

    let source = Vault::new(&vault.address, "secret", "myapp/loads")
        .with_key("db")
        .with_token(TOKEN);

    let fetched = source.fetch().unwrap();
    let sources = [dynamic_config::Source::inline(
        &fetched.text,
        fetched.format,
    )];

    let db: Db = dynamic_config::load(&dynamic_config::LoadSpec::new("db", &sources))
        .expect("the fetched document is shaped like a section");

    assert_eq!(
        db,
        Db {
            host: "db.internal".to_owned(),
            port: 6432,
        }
    );
}

#[test]
fn a_wrong_token_is_a_remote_error_not_a_panic() {
    let vault = vault_with("myapp/guarded", serde_json::json!({ "host": "x" }));

    let source = Vault::new(&vault.address, "secret", "myapp/guarded")
        .with_key("db")
        .with_token("not-the-token");

    let error = source.fetch().expect_err("the token is wrong");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    // `describe()` now leads with the address, so the mount is preceded by
    // it rather than by the word "vault" directly.
    assert!(
        error.to_string().contains("secret/myapp/guarded"),
        "{error}"
    );
    assert!(error.to_string().contains(&vault.address), "{error}");
}

#[test]
fn a_path_with_no_secret_says_so() {
    let vault = vault_with("myapp/present", serde_json::json!({ "host": "x" }));

    let source = Vault::new(&vault.address, "secret", "myapp/absent")
        .with_key("db")
        .with_token(TOKEN);

    let error = source.fetch().expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

use std::sync::mpsc;
use std::time::Duration;

use dynamic_config::RemoteWatch;

/// Writes a new version of a secret.
fn write(address: &str, path: &str, secret: serde_json::Value) {
    let response = ureq::post(&format!("{address}/v1/secret/data/{path}"))
        .header("X-Vault-Token", TOKEN)
        .send_json(serde_json::json!({ "data": secret }))
        .expect("writing the secret should succeed");

    assert!(response.status().is_success(), "{}", response.status());
}

const TICK: Duration = Duration::from_millis(300);

/// A new version reaches the callback.
#[test]
fn a_new_version_reaches_the_callback() {
    let vault = vault_with("watched", serde_json::json!({ "host": "first" }));

    let source = Vault::new(&vault.address, "secret", "watched").with_token(TOKEN);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, TICK, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // The first tick records the current version without firing, so writing
    // into that window would prime the loop with the new value and the change
    // under test would never happen.
    std::thread::sleep(TICK * 3);

    write(
        &vault.address,
        "watched",
        serde_json::json!({ "host": "second" }),
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("a version bump should be noticed");

    assert_eq!(document.format, Format::Json);
    assert!(document.text.contains("second"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// The version present at startup is not a change, so it is not reported.
#[test]
fn the_starting_version_is_not_announced() {
    let vault = vault_with("quiet", serde_json::json!({ "host": "first" }));

    let source = Vault::new(&vault.address, "secret", "quiet").with_token(TOKEN);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, TICK, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    assert!(
        receiver.recv_timeout(TICK * 6).is_err(),
        "a watch reports changes; announcing the current value would make every \
         restart look like an edit"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// Stopping is noticed inside the interval, not after it — a long poll should
/// not mean a long exit.
#[test]
fn stopping_does_not_wait_out_the_interval() {
    let vault = vault_with("stopped", serde_json::json!({ "host": "first" }));

    // Deliberately far longer than the assertion below allows.
    let source = Vault::new(&vault.address, "secret", "stopped").with_token(TOKEN);

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread =
        std::thread::spawn(move || source.watch(&watching, Duration::from_secs(60), |_| Ok(())));

    std::thread::sleep(Duration::from_millis(500));

    let stopped = std::time::Instant::now();
    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");

    assert!(
        stopped.elapsed() < Duration::from_secs(5),
        "stopping took {:?}; a sixty-second poll must not mean a sixty-second exit",
        stopped.elapsed()
    );
}

/// A secret the program cannot read is not a reason to give up watching.
#[test]
fn a_failing_check_does_not_end_the_watch() {
    let vault = vault_with("resilient", serde_json::json!({ "host": "first" }));

    // A bad token, so every metadata check fails.
    let source = Vault::new(&vault.address, "secret", "resilient").with_token("not-the-token");

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let finished = std::sync::Arc::clone(&running);

    let thread = std::thread::spawn(move || {
        let outcome = source.watch(&watching, TICK, |_| Ok(()));

        finished.store(false, std::sync::atomic::Ordering::SeqCst);

        outcome
    });

    std::thread::sleep(TICK * 5);

    assert!(
        running.load(std::sync::atomic::Ordering::SeqCst),
        "an unreadable secret is exactly what a watch is supposed to survive"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

// ---------------------------------------------------------------------------
// Logging in
// ---------------------------------------------------------------------------

use dynamic_config_vault::Auth;

/// Enables an auth method and configures it, with the root token.
fn post(address: &str, path: &str, body: serde_json::Value) -> serde_json::Value {
    let mut response = ureq::post(&format!("{address}/v1/{path}"))
        .header("X-Vault-Token", TOKEN)
        .send_json(&body)
        .unwrap_or_else(|error| panic!("POST {path} should succeed: {error}"));

    assert!(response.status().is_success(), "{}", response.status());

    // A 204 has no body, which is not an error here.
    response
        .body_mut()
        .read_json()
        .unwrap_or(serde_json::Value::Null)
}

/// Reads with the root token. `role-id` is a GET in Vault's AppRole API, while
/// `secret-id` is a POST — a distinction worth keeping straight rather than
/// papering over.
fn get(address: &str, path: &str) -> serde_json::Value {
    ureq::get(&format!("{address}/v1/{path}"))
        .header("X-Vault-Token", TOKEN)
        .call()
        .unwrap_or_else(|error| panic!("GET {path} should succeed: {error}"))
        .body_mut()
        .read_json()
        .expect("Vault answers JSON")
}

/// Grants read access to `secret/` and nothing else.
fn grant_reader(address: &str) {
    post(
        address,
        "sys/policy/reader",
        serde_json::json!({
            "policy": r#"path "secret/*" { capabilities = ["read", "list"] }"#
        }),
    );
}

#[test]
fn app_role_logs_in_and_reads() {
    let vault = vault_with("approle", serde_json::json!({ "host": "by-approle" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/approle",
        serde_json::json!({ "type": "approle" }),
    );
    post(
        &vault.address,
        "auth/approle/role/app",
        serde_json::json!({ "token_policies": "reader", "token_ttl": "10m" }),
    );

    let role_id = get(&vault.address, "auth/approle/role/app/role-id");
    let role_id = role_id["data"]["role_id"].as_str().expect("a role id");

    let secret_id = post(
        &vault.address,
        "auth/approle/role/app/secret-id",
        serde_json::json!({}),
    );
    let secret_id = secret_id["data"]["secret_id"]
        .as_str()
        .expect("a secret id");

    let source = Vault::new(&vault.address, "secret", "approle")
        .with_auth(Auth::app_role(role_id, secret_id));

    let fetched = source.fetch().expect("approle should get a usable token");

    assert!(fetched.text.contains("by-approle"), "{}", fetched.text);
}

#[test]
fn userpass_puts_the_name_in_the_path_and_the_password_in_the_body() {
    let vault = vault_with("userpass", serde_json::json!({ "host": "by-userpass" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/userpass",
        serde_json::json!({ "type": "userpass" }),
    );
    post(
        &vault.address,
        "auth/userpass/users/alice",
        serde_json::json!({ "password": "hunter2", "token_policies": "reader" }),
    );

    let source = Vault::new(&vault.address, "secret", "userpass")
        .with_auth(Auth::userpass("alice", "hunter2"));

    let fetched = source.fetch().expect("userpass should get a usable token");

    assert!(fetched.text.contains("by-userpass"), "{}", fetched.text);
}

#[test]
fn a_method_mounted_somewhere_else_is_found_there() {
    let vault = vault_with("mounted", serde_json::json!({ "host": "by-mount" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/userpass-prod",
        serde_json::json!({ "type": "userpass" }),
    );
    post(
        &vault.address,
        "auth/userpass-prod/users/bob",
        serde_json::json!({ "password": "hunter2", "token_policies": "reader" }),
    );

    let source = Vault::new(&vault.address, "secret", "mounted")
        .with_auth(Auth::userpass("bob", "hunter2").at_mount("userpass-prod"));

    assert!(
        source.fetch().is_ok(),
        "mounting a method twice is ordinary Vault practice"
    );
}

#[test]
fn bad_credentials_say_which_method_refused_them() {
    let vault = vault_with("refused", serde_json::json!({ "host": "unreachable" }));

    post(
        &vault.address,
        "sys/auth/userpass",
        serde_json::json!({ "type": "userpass" }),
    );

    let source = Vault::new(&vault.address, "secret", "refused")
        .with_auth(Auth::userpass("nobody", "wrong"));

    let error = source.fetch().expect_err("there is no such user");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("userpass"), "{error}");
}

#[test]
fn no_credentials_at_all_says_what_to_call() {
    let vault = vault_with("anonymous", serde_json::json!({ "host": "unreachable" }));

    let source = Vault::new(&vault.address, "secret", "anonymous");

    let error = source.fetch().expect_err("nothing was configured");

    assert!(error.to_string().contains("with_auth"), "{error}");
}

/// One login serves many reads: a configuration source that logged in per fetch
/// would turn a refresh loop into a login storm, and fill the audit log with it.
#[test]
fn the_token_is_reused_across_reads() {
    let vault = vault_with("reused", serde_json::json!({ "host": "by-approle" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/approle",
        serde_json::json!({ "type": "approle" }),
    );
    post(
        &vault.address,
        "auth/approle/role/app",
        serde_json::json!({
            "token_policies": "reader",
            "token_ttl": "10m",
            // One login only: a second would be refused, which is how the test
            // proves the token was reused rather than re-fetched.
            "secret_id_num_uses": 1,
        }),
    );

    let role_id = get(&vault.address, "auth/approle/role/app/role-id");
    let secret_id = post(
        &vault.address,
        "auth/approle/role/app/secret-id",
        serde_json::json!({}),
    );

    let source = Vault::new(&vault.address, "secret", "reused").with_auth(Auth::app_role(
        role_id["data"]["role_id"].as_str().unwrap(),
        secret_id["data"]["secret_id"].as_str().unwrap(),
    ));

    assert!(source.fetch().is_ok(), "the first read logs in");
    assert!(
        source.fetch().is_ok(),
        "the second must reuse the token: the secret id is spent"
    );
    assert!(source.fetch().is_ok());
}

/// A lease shorter than the refresh window means every read has to renew or log
/// in again — which is the whole expiry path, exercised deterministically
/// rather than by waiting for a real token to age out.
#[test]
fn a_source_keeps_working_past_the_life_of_its_first_token() {
    let vault = vault_with("expiring", serde_json::json!({ "host": "by-approle" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/approle",
        serde_json::json!({ "type": "approle" }),
    );
    post(
        &vault.address,
        "auth/approle/role/app",
        serde_json::json!({
            "token_policies": "reader",
            // Far below the thirty-second refresh window, so the token is stale
            // the moment it arrives.
            "token_ttl": "2s",
            "token_max_ttl": "10s",
        }),
    );

    let role_id = get(&vault.address, "auth/approle/role/app/role-id");
    let secret_id = post(
        &vault.address,
        "auth/approle/role/app/secret-id",
        serde_json::json!({}),
    );

    let source = Vault::new(&vault.address, "secret", "expiring").with_auth(Auth::app_role(
        role_id["data"]["role_id"].as_str().unwrap(),
        secret_id["data"]["secret_id"].as_str().unwrap(),
    ));

    for round in 0..4 {
        let fetched = source
            .fetch()
            .unwrap_or_else(|error| panic!("read {round} should survive the lease: {error}"));

        assert!(fetched.text.contains("by-approle"));

        std::thread::sleep(Duration::from_millis(600));
    }
}

/// Credentials that are gone stay gone: reported, not retried until something
/// times out.
#[test]
fn credentials_that_stop_working_are_reported() {
    let vault = vault_with("gone", serde_json::json!({ "host": "by-approle" }));

    grant_reader(&vault.address);
    post(
        &vault.address,
        "sys/auth/approle",
        serde_json::json!({ "type": "approle" }),
    );
    post(
        &vault.address,
        "auth/approle/role/app",
        serde_json::json!({ "token_policies": "reader", "token_ttl": "10m" }),
    );

    let role_id = get(&vault.address, "auth/approle/role/app/role-id");
    let secret_id = post(
        &vault.address,
        "auth/approle/role/app/secret-id",
        serde_json::json!({}),
    );
    let secret = secret_id["data"]["secret_id"].as_str().unwrap().to_owned();

    let source = Vault::new(&vault.address, "secret", "gone").with_auth(Auth::app_role(
        role_id["data"]["role_id"].as_str().unwrap(),
        &secret,
    ));

    assert!(source.fetch().is_ok(), "the first read logs in");

    // An operator rotates the credentials out from under the process.
    post(
        &vault.address,
        "auth/approle/role/app/secret-id/destroy",
        serde_json::json!({ "secret_id": secret }),
    );

    // The cached token still works, so reads keep succeeding — which is the
    // point of caching it.
    assert!(
        source.fetch().is_ok(),
        "a destroyed secret id does not invalidate a token already issued"
    );

    // But a source starting from nothing has no way in, and says so promptly.
    let fresh = Vault::new(&vault.address, "secret", "gone").with_auth(Auth::app_role(
        role_id["data"]["role_id"].as_str().unwrap(),
        &secret,
    ));

    let error = fresh.fetch().expect_err("the secret id was destroyed");

    assert!(error.to_string().contains("approle"), "{error}");
}

/// A deleted secret is not a configuration change: the version counter does
/// not move, nothing is delivered, and the watch stays alive for the secret's
/// next version.
#[test]
fn a_deleted_secret_is_not_reported_as_a_change() {
    let vault = vault_with("doomed", serde_json::json!({ "host": "first" }));

    let source = Vault::new(&vault.address, "secret", "doomed").with_token(TOKEN);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = std::sync::mpsc::channel();

    let address = vault.address.clone();
    let thread = std::thread::spawn(move || {
        source.watch(&watching, TICK, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // Let the first tick record the version, then soft-delete the secret.
    std::thread::sleep(std::time::Duration::from_millis(700));

    let response = ureq::delete(&format!("{address}/v1/secret/data/doomed"))
        .header("X-Vault-Token", TOKEN)
        .call()
        .expect("deleting the secret should succeed");
    assert!(response.status().is_success());

    let quiet = receiver.recv_timeout(std::time::Duration::from_secs(2));
    assert!(
        quiet.is_err(),
        "no configuration is not a configuration; a deletion must not reach the callback"
    );

    // A new version IS a change, which also proves the loop survived.
    write(&address, "doomed", serde_json::json!({ "host": "back" }));

    let document = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the next version should be noticed");
    assert!(document.text.contains("back"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}
