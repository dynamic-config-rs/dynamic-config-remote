//! Against a real Redis, in a container.
//!
//! ```text
//! cargo test -p dynamic-config-redis
//! ```
//!
//! Needs a working Docker daemon. Without one these fail rather than skipping:
//! a test that quietly stops running is one nobody notices has stopped.

use std::sync::mpsc;
use std::time::Duration;

use dynamic_config::{Format, RemoteSource, RemoteWatch};
use dynamic_config_redis::Redis;
use redis::Commands;
use testcontainers::ImageExt;
use testcontainers_modules::redis::Redis as RedisImage;

/// The module defaults to Redis 5, which is long out of support. Pinned to a
/// current one: keyspace notifications behave the same, and a test should not
/// be the last thing in the world exercising a decade-old server.
const TAG: &str = "7-alpine";

struct Running {
    url: String,
    _container: testcontainers::Container<RedisImage>,
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

/// Starts Redis and writes one key into it.
fn redis_with(key: &str, value: &str) -> Running {
    let container = start_resilient(|| RedisImage::default().with_tag(TAG));

    let port = container
        .get_host_port_ipv4(6379)
        .expect("Redis should expose its port");
    let url = format!("redis://127.0.0.1:{port}");

    let mut connection = redis::Client::open(url.as_str())
        .unwrap()
        .get_connection()
        .expect("the container should accept a connection");

    let _: () = connection
        .set(key, value)
        .expect("writing the key succeeds");

    Running {
        url,
        _container: container,
    }
}

fn write(url: &str, key: &str, value: &str) {
    let mut connection = redis::Client::open(url).unwrap().get_connection().unwrap();
    let _: () = connection.set(key, value).unwrap();
}

fn enable_notifications(url: &str) {
    let mut connection = redis::Client::open(url).unwrap().get_connection().unwrap();
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query(&mut connection)
        .expect("the server accepts the setting");
}

#[test]
fn a_key_holds_a_whole_configuration_document() {
    let redis = redis_with(
        "myapp/db.json",
        r#"{"db": {"host": "localhost", "port": 5432}}"#,
    );

    let source = Redis::new(&redis.url, "myapp/db.json").expect("the url parses");
    let fetched = source.fetch().expect("the key is there");

    assert_eq!(fetched.format, Format::Json, "from the key's extension");
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

    let redis = redis_with(
        "myapp/loads.json",
        r#"{"db": {"host": "db.internal", "port": 6432}}"#,
    );

    let source = Redis::new(&redis.url, "myapp/loads.json").unwrap();
    let fetched = source.fetch().unwrap();

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
            port: 6432
        }
    );
}

#[test]
fn a_key_that_is_not_there_is_a_remote_error() {
    let redis = redis_with("myapp/present.json", r#"{"db": {}}"#);

    let source = Redis::new(&redis.url, "myapp/absent.json").unwrap();
    let error = source.fetch().expect_err("nothing is stored there");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("holds no value"), "{error}");
}

#[test]
fn an_unreachable_server_fails_at_the_first_read() {
    // Port 1 is reserved and nothing serves Redis on it. The client is built
    // lazily, so this is where it surfaces.
    let source = Redis::new("redis://127.0.0.1:1", "myapp/db.json").expect("the url parses");

    assert_eq!(
        source.fetch().expect_err("nothing is listening").kind(),
        dynamic_config::ErrorKind::Remote
    );
}

/// Keyspace notifications are off by default. A watch that waited for them
/// anyway would hang, which is the worst way for a feature to be unavailable.
// ---------------------------------------------------------------------------
// Several keys as one document
// ---------------------------------------------------------------------------
use dynamic_config::Value;
use dynamic_config_redis::Keys;

/// Starts Redis and writes every `(key, value)` pair into it.
fn redis_holding(pairs: &[(&str, &str)]) -> Running {
    let running = redis_with(pairs[0].0, pairs[0].1);

    for (key, value) in &pairs[1..] {
        write(&running.url, key, value);
    }

    running
}

/// The rule a named list inherits from a list of `.file(..)` calls: call
/// order, and the later key wins.
#[test]
fn named_keys_merge_in_call_order_and_the_later_one_wins() {
    let redis = redis_holding(&[
        (
            "myapp/base.json",
            r#"{"db": {"host": "base", "port": 5432}}"#,
        ),
        ("myapp/local.json", r#"{"db": {"port": 6432}}"#),
    ]);

    let source = Redis::new(
        &redis.url,
        Keys::several(["myapp/base.json", "myapp/local.json"]),
    )
    .expect("the container is up");

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

/// The other half, through a real `SCAN` cursor rather than a scripted one.
#[test]
fn a_prefix_folds_its_sections_into_one_document() {
    let redis = redis_holding(&[
        ("myapp/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("myapp/server.json", r#"{"server": {"port": 8080}}"#),
        // Outside the prefix: proof the match is bounded where it says.
        ("other/db.json", r#"{"db": {"host": "wrong"}}"#),
    ]);

    let source = Redis::new(&redis.url, Keys::prefix("myapp/"))
        .unwrap()
        .with_format(Format::Json);

    let fetched = source.fetch().expect("the scan finished");
    let merged = Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("db.internal".to_owned()))
    );
    assert_eq!(merged.get("server.port"), Some(&Value::Integer(8080)));
}

/// `MATCH` takes a glob, so a prefix with a bracket in it would select keys
/// nobody asked for. Against a real server, because the escaping is only worth
/// anything if Redis reads it the way this crate writes it.
#[test]
fn a_prefix_containing_glob_characters_matches_itself_and_nothing_else() {
    let redis = redis_holding(&[
        ("my[a]pp/db.json", r#"{"db": {"host": "literal"}}"#),
        // What the unescaped glob `my[a]pp/*` would have matched instead.
        ("myapp/db.json", r#"{"db": {"host": "glob"}}"#),
    ]);

    let source = Redis::new(&redis.url, Keys::prefix("my[a]pp/"))
        .unwrap()
        .with_format(Format::Json);

    let fetched = source.fetch().expect("the literal prefix matched");

    assert!(fetched.text.contains("literal"), "{}", fetched.text);
    assert!(
        !fetched.text.contains("glob"),
        "a prefix means the prefix, not what a glob would make of it: {}",
        fetched.text
    );
}

/// A prefix says "these are disjoint sections". Two of them supplying one path
/// is a deployment bug, named rather than resolved — by path, never by value.
#[test]
fn two_keys_under_a_prefix_supplying_one_path_are_refused_by_name() {
    let redis = redis_holding(&[
        ("clash/a.json", r#"{"db": {"password": "hunter2-first"}}"#),
        ("clash/b.json", r#"{"db": {"password": "hunter2-second"}}"#),
    ]);

    let source = Redis::new(&redis.url, Keys::prefix("clash/"))
        .unwrap()
        .with_format(Format::Json);

    let error = source.fetch().expect_err("both keys supply db.password");
    let printed = format!("{error} {error:?}");

    assert!(printed.contains("clash/a.json"), "{printed}");
    assert!(printed.contains("clash/b.json"), "{printed}");
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
    let redis = redis_holding(&[("partial/db.json", r#"{"db": {"host": "here"}}"#)]);

    let source = Redis::new(
        &redis.url,
        Keys::several(["partial/db.json", "partial/absent.json"]),
    )
    .unwrap();

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

    let redis = redis_holding(&[
        ("split/host.json", r#"{"db": {"host": "db.internal"}}"#),
        ("split/port.json", r#"{"db": {"port": 6432}}"#),
    ]);

    let source = Redis::new(&redis.url, Keys::prefix("split/"))
        .unwrap()
        .with_format(Format::Json);
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

#[test]
fn a_watch_reports_when_notifications_are_off_rather_than_hanging() {
    let redis = redis_with("myapp/quiet.json", r#"{"db": {}}"#);

    let source = Redis::new(&redis.url, "myapp/quiet.json").unwrap();
    let watch = RemoteWatch::new();

    let error = source
        .watch(&watch.watching(), |_| Ok(()))
        .expect_err("nothing would ever arrive");

    assert!(
        error.to_string().contains("notify-keyspace-events"),
        "{error}"
    );
}

#[test]
fn a_change_reaches_the_callback() {
    let redis = redis_with("myapp/watched.json", r#"{"db": {"host": "first"}}"#);

    enable_notifications(&redis.url);

    let source = Redis::new(&redis.url, "myapp/watched.json").unwrap();
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    // Let the subscription settle: a write before it exists publishes to nobody.
    std::thread::sleep(Duration::from_millis(500));

    write(
        &redis.url,
        "myapp/watched.json",
        r#"{"db": {"host": "second"}}"#,
    );

    let document = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("a keyspace notification should arrive");

    assert!(document.text.contains("second"), "{}", document.text);

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// A deleted key is not a configuration, so it is not pushed through.
#[test]
fn a_deletion_is_not_reported_as_a_change() {
    let redis = redis_with("myapp/deleted.json", r#"{"db": {"host": "first"}}"#);

    enable_notifications(&redis.url);

    let source = Redis::new(&redis.url, "myapp/deleted.json").unwrap();
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document);
            Ok(())
        })
    });

    std::thread::sleep(Duration::from_millis(500));

    let mut connection = redis::Client::open(redis.url.as_str())
        .unwrap()
        .get_connection()
        .unwrap();
    let _: () = connection.del("myapp/deleted.json").unwrap();

    assert!(
        receiver.recv_timeout(Duration::from_secs(2)).is_err(),
        "no configuration is not a configuration"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

// ---------------------------------------------------------------------------
// Watching a named list
// ---------------------------------------------------------------------------

/// Writes every key of the set in one `MSET`.
///
/// One command on purpose: it takes a torn *write* off the table, so a delivery
/// whose halves disagree can only be a torn *read* — which is the thing under
/// test.
fn stamp_together(url: &str, generation: u64) {
    let mut connection = redis::Client::open(url).unwrap().get_connection().unwrap();

    let _: () = redis::cmd("MSET")
        .arg("stamped/db.json")
        .arg(format!(r#"{{"db": {{"generation": {generation}}}}}"#))
        .arg("stamped/server.json")
        .arg(format!(r#"{{"server": {{"generation": {generation}}}}}"#))
        .query(&mut connection)
        .expect("the set is written as one command");
}

/// The property a watch on a set exists to have: **every delivery agrees with
/// itself**.
///
/// One generation is stamped into both keys of the set with one `MSET`, so
/// every state the server ever holds has the two halves equal. A watch that
/// woke on a notification and then read the set key by key would eventually
/// deliver `db.generation` from one write beside `server.generation` from
/// another — a document that never existed at any instant. `MGET` is one
/// command and Redis runs one command as one operation, so there is no window
/// for that, and this is the test that would catch the window opening.
///
/// The assertion is on the highest generation rather than on a count: the read
/// follows the notification rather than being simultaneous with it, so a
/// delivery may carry a *newer* state than the write that woke it, and two
/// writes may coalesce into one delivery. Spurious, never torn.
#[test]
fn a_named_list_watch_never_delivers_a_document_that_never_existed() {
    let redis = redis_holding(&[
        ("stamped/db.json", r#"{"db": {"generation": 0}}"#),
        ("stamped/server.json", r#"{"server": {"generation": 0}}"#),
    ]);

    enable_notifications(&redis.url);

    let source = Redis::new(
        &redis.url,
        Keys::several(["stamped/db.json", "stamped/server.json"]),
    )
    .expect("the container is up");

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document.text);
            Ok(())
        })
    });

    // Let the subscriptions settle: a write before they exist publishes to
    // nobody.
    std::thread::sleep(Duration::from_millis(500));

    for generation in 1..=6 {
        stamp_together(&redis.url, generation);

        std::thread::sleep(Duration::from_millis(150));
    }

    let mut seen = 0;
    let mut highest = 0;

    while let Ok(text) = receiver.recv_timeout(Duration::from_secs(10)) {
        let merged = Value::parse(&text, Format::Json).expect("every delivery is a document");

        let db = merged.get("db.generation").cloned();
        let server = merged.get("server.generation").cloned();

        assert_eq!(
            db, server,
            "a delivery whose two halves disagree is a document that never \
             existed at any instant: {text}"
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

/// A watch on a set has to hear about **every** key in it. Subscribing to the
/// first key only would pass every test that writes the whole set together, and
/// then go quiet the day a deployment changed one section.
#[test]
fn a_change_to_any_key_of_the_set_reaches_the_callback() {
    let redis = redis_holding(&[
        ("listed/db.json", r#"{"db": {"host": "first"}}"#),
        ("listed/server.json", r#"{"server": {"port": 8080}}"#),
    ]);

    enable_notifications(&redis.url);

    let source = Redis::new(
        &redis.url,
        Keys::several(["listed/db.json", "listed/server.json"]),
    )
    .expect("the container is up");

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

    // The *last* key of the list, which is the one a watch on the first would
    // never hear about.
    write(
        &redis.url,
        "listed/server.json",
        r#"{"server": {"port": 9090}}"#,
    );

    let text = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("a keyspace notification should arrive for the second key");

    let merged = Value::parse(&text, Format::Json).expect("a document");

    assert_eq!(
        merged.get("server.port"),
        Some(&Value::Integer(9090)),
        "the change is delivered"
    );
    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("first".to_owned())),
        "and so is the rest of the set, which is what the callback installs"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}

/// A prefix is refused at `watch()`, before the first event rather than after a
/// bad one — because re-finding the keys is a `SCAN`, and a cursor is many
/// commands with writes free to land between them.
#[test]
fn a_prefix_watch_is_refused_against_a_real_server_too() {
    let redis = redis_holding(&[
        ("scanned/db.json", r#"{"db": {"host": "first"}}"#),
        ("scanned/server.json", r#"{"server": {"port": 8080}}"#),
    ]);

    enable_notifications(&redis.url);

    let source = Redis::new(&redis.url, Keys::prefix("scanned/"))
        .expect("the container is up")
        .with_format(Format::Json);

    let watch = RemoteWatch::new();
    let error = source
        .watch(&watch.watching(), |_| Ok(()))
        .expect_err("a prefix cannot be watched");

    assert!(error.to_string().contains("cannot be watched"), "{error}");
    assert!(error.to_string().contains("Keys::several"), "{error}");
}

#[test]
fn a_source_can_share_a_client_the_program_already_has() {
    let redis = redis_with("myapp/shared.json", r#"{"db": {"host": "shared"}}"#);

    let client = redis::Client::open(redis.url.as_str()).expect("the program's own client");
    let source = Redis::from_client(client, "myapp/shared.json");

    assert!(source.fetch().unwrap().text.contains("shared"));
    assert!(
        source.describe().contains("an existing client"),
        "an error should not name a server this source never dialled: {}",
        source.describe()
    );
}

// ---------------------------------------------------------------------------
// Reporting a failing watch
//
// One `#[dynamic_config]` type per test: the snapshot, the remote slot and the
// sink's generation all live in statics keyed by the type, so two tests
// sharing one would race and — worse — pass alone.
// ---------------------------------------------------------------------------

/// Waits for a subscriber to exist, then kills its connection.
///
/// `CLIENT KILL` rather than stopping the container: it is the failure this
/// watch actually meets — a restart, a proxy reaping an idle connection, an
/// operator tidying up — and it leaves the server there to be asked what it
/// thinks afterwards. Waiting first matters: a kill that lands before the
/// `SUBSCRIBE` kills nothing and the test would prove nothing.
fn kill_the_subscriber(url: &str) {
    let mut connection = redis::Client::open(url).unwrap().get_connection().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);

    loop {
        let listed: String = redis::cmd("CLIENT")
            .arg("LIST")
            .arg("TYPE")
            .arg("pubsub")
            .query(&mut connection)
            .expect("the server lists its clients");

        if !listed.trim().is_empty() {
            break;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "no subscriber ever connected, so there is nothing to kill"
        );

        std::thread::sleep(Duration::from_millis(50));
    }

    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query(&mut connection)
        .expect("the server kills its clients");

    assert!(killed > 0, "the subscription should have been killed");
}

/// The failure nobody notices: the loop was working, the subscription died,
/// and the watch ends on a thread whose result is usually dropped.
/// Configuration simply stops updating, and until this landed the status said
/// the store was fine — because the last thing it heard about was a delivery.
///
/// What it must say afterwards is a *pair*: `reachable()` goes to
/// `Some(false)` while `last_fetch` keeps the instant the last document really
/// arrived, so an alert can ask "down, and stale for how long". A failure that
/// reset the clock would hide the second half.
#[test]
fn a_dead_subscription_reports_the_store_as_down_and_leaves_the_clock_running() {
    use dynamic_config::dynamic_config;

    #[dynamic_config]
    #[derive(Debug, serde::Deserialize)]
    struct Killed {
        // Never read: this test is about the status the store records, not
        // about the document.
        #[allow(dead_code)]
        host: String,
    }

    let redis = redis_with("killed/db.json", r#"{"db": {"host": "first"}}"#);

    enable_notifications(&redis.url);

    Killed::set_remote(Redis::new(&redis.url, "killed/db.json").unwrap());
    Killed::refresh_remote().expect("the store answers the first read");

    // Taken after the source is installed, which is what fences it.
    let sink = Killed::remote_sink();
    let before = sink.status();

    assert_eq!(before.reachable(), Some(true), "one fetch, and it answered");
    assert!(before.last_fetch.is_some());

    let watcher = Redis::new(&redis.url, "killed/db.json")
        .unwrap()
        .reporting_to(sink);
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (ended, ending) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        let outcome = watcher.watch(&watching, |_| Ok(()));
        let _ = ended.send(());
        outcome
    });

    kill_the_subscriber(&redis.url);

    ending
        .recv_timeout(Duration::from_secs(20))
        .expect("a subscription that died ends the watch rather than spinning");

    let error = thread
        .join()
        .expect("the thread should end")
        .expect_err("the subscription was killed underneath it");

    watch.stop();

    assert!(
        error.to_string().contains("the subscription failed"),
        "{error}"
    );

    let after = sink.status();

    assert_eq!(
        after.reachable(),
        Some(false),
        "a loop that stopped reaching its store is a store that is down"
    );
    assert_eq!(after.consecutive_failures, 1);
    assert_eq!(
        after.last_fetch, before.last_fetch,
        "the staleness clock keeps running: `last_fetch` is when a document \
         last arrived, and a failed attempt is not one"
    );
    assert_eq!(
        after.fetches, before.fetches,
        "a failure is not a fetch, however it is counted elsewhere"
    );
    assert_eq!(
        after
            .last_failure
            .as_ref()
            .expect("a failure was recorded")
            .kind,
        dynamic_config::ErrorKind::Remote,
        "a subscription that dropped may yet come back"
    );
}

/// The other shape, against a real server: a re-read that failed does not end
/// the watch, so nothing else will ever mention it — and the streak is what
/// separates a blip from a store that has really stopped answering, because a
/// delivery clears it.
///
/// A member of the set going missing is the honest way to make one `MGET`
/// fail: the notification arrives, the read is one command, and it comes back
/// with a nil where a document should be.
#[test]
fn a_failed_re_read_reports_and_the_streak_clears_when_the_key_comes_back() {
    use dynamic_config::dynamic_config;

    #[dynamic_config]
    #[derive(Debug, serde::Deserialize)]
    struct Halved {
        // Never read, for the reason the test above gives.
        #[allow(dead_code)]
        host: String,
    }

    let redis = redis_holding(&[
        ("halved/db.json", r#"{"db": {"host": "first"}}"#),
        ("halved/server.json", r#"{"server": {"port": 8080}}"#),
    ]);

    enable_notifications(&redis.url);

    let keys = Keys::several(["halved/db.json", "halved/server.json"]);

    Halved::set_remote(Redis::new(&redis.url, keys.clone()).unwrap());
    Halved::refresh_remote().expect("both keys answer");
    // No files: the fetched document is the whole configuration, and
    // initializing through the builder is what lets the sink reload later.
    Halved::builder("db")
        .init()
        .expect("the fetched document is a configuration");

    let sink = Halved::remote_sink();
    let before = sink.status();

    assert_eq!(before.reachable(), Some(true));

    let watcher = Redis::new(&redis.url, keys).unwrap().reporting_to(sink);
    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        watcher.watch(&watching, move |document| {
            let _ = sender.send(document.text.clone());

            // The delivery half of the same sink: `apply` records the fetch
            // that clears the streak `failed` has been climbing, which is what
            // makes the two halves one story rather than two counters.
            sink.apply(document).or(Ok(()))
        })
    });

    // Let the subscription settle: a write before it exists publishes to
    // nobody.
    std::thread::sleep(Duration::from_millis(500));

    let mut connection = redis::Client::open(redis.url.as_str())
        .unwrap()
        .get_connection()
        .unwrap();
    // A deletion is not a change, so this alone delivers nothing and reports
    // nothing. It is the write after it that finds the set half gone.
    let _: () = connection.del("halved/server.json").unwrap();

    write(
        &redis.url,
        "halved/db.json",
        r#"{"db": {"host": "second"}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);

    while sink.status().consecutive_failures == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let failing = sink.status();

    assert_eq!(
        failing.reachable(),
        Some(false),
        "the notification arrived and the document did not"
    );
    assert_eq!(
        failing.last_fetch, before.last_fetch,
        "a failed re-read leaves the clock where the last document left it"
    );
    assert_eq!(failing.fetches, before.fetches);
    assert!(
        receiver.recv_timeout(Duration::from_millis(500)).is_err(),
        "a failed re-read delivers nothing"
    );

    // The loop is still alive, which is the whole reason this failure needed
    // reporting — and the key coming back is a delivery, which is what a
    // *streak* means: one failure between deliveries clears.
    write(
        &redis.url,
        "halved/server.json",
        r#"{"server": {"port": 9090}}"#,
    );

    let text = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the set is whole again, so the watch delivers again");

    assert!(text.contains("9090"), "{text}");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);

    while sink.status().consecutive_failures > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let recovered = sink.status();

    assert_eq!(
        recovered.consecutive_failures, 0,
        "a delivery is what clears the streak, and `apply` is the delivery"
    );
    assert_eq!(
        recovered.reachable(),
        Some(true),
        "the store is answering again, and the same status says so"
    );
    assert!(
        recovered.last_fetch > before.last_fetch,
        "the clock moves on a delivery and not on a failure"
    );

    watch.stop();
    thread
        .join()
        .expect("the loop should end")
        .expect("cleanly");
}
