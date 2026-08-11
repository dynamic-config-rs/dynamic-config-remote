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
