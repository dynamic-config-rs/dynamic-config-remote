//! Unplugging Redis while a watch is running.
//!
//! ```text
//! just chaos
//! ```
//!
//! Every test here is `#[ignore]`d: they need Docker *and* a second
//! container, they take tens of seconds, and they belong to a nightly and a
//! release gate rather than to every commit. `cargo test -p
//! dynamic-config-redis` still compiles them, which is what keeps them from
//! rotting.
//!
//! # Why a proxy rather than stopping the container
//!
//! Because a stopped container comes back on a **different host port**:
//! Docker re-publishes a randomly mapped port on restart, so the source
//! would be pointing at nothing and "it recovered" could never be asserted.
//! [toxiproxy] sits between the test and a server that never restarts, and
//! disabling the proxy resets every connection through it — which is what
//! a cut cable looks like from inside a subscription.
//!
//! What these prove is the pair an alert reads: `remote_up` goes to zero
//! **while the staleness clock keeps running**, and the document that was
//! serving before the outage is still serving after it.
//!
//! [toxiproxy]: https://github.com/Shopify/toxiproxy

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dynamic_config::{dynamic_config, RemoteSink, RemoteWatch};
use dynamic_config_redis::Redis;
use redis::Commands;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use testcontainers_modules::redis::Redis as RedisImage;

const TAG: &str = "7-alpine";
const TOXIPROXY: &str = "2.12.0";
/// The port the proxy listens on *inside* its container. Published to a
/// random host port, which is what the source is pointed at — and it stays
/// put for the whole test, because nothing restarts.
const LISTEN: u16 = 8666;

/// A store, a proxy in front of it, and the switch between them.
struct Chaos {
    /// What to point a source at: the proxy's published address.
    url: String,
    /// The toxiproxy control API, on the host.
    api: String,
    name: String,
    _store: Container<RedisImage>,
    _proxy: Container<GenericImage>,
}

impl Chaos {
    /// Cuts every connection through the proxy, and refuses new ones.
    fn unplug(&self) {
        self.control(r#"{"enabled":false}"#);
    }

    /// Puts the cable back.
    fn plug_in(&self) {
        self.control(r#"{"enabled":true}"#);
    }

    fn control(&self, body: &str) {
        post(&self.api, &format!("/proxies/{}", self.name), body);
    }
}

/// One HTTP POST, written by hand.
///
/// The toxiproxy API is three fields of JSON over plain HTTP to a port on
/// this machine; a client library for that would be a dependency in the
/// manifest of a crate that ships none of it.
fn post(host: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(host).expect("the proxy's API is listening");

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream.write_all(request.as_bytes()).unwrap();

    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();

    assert!(
        answer.starts_with("HTTP/1.1 2"),
        "the proxy refused {path}: {answer}"
    );

    answer
}

/// Redis behind a proxy, with `key` already written into it.
fn chaos_with(key: &str, value: &str) -> Chaos {
    // Unique per test, so two of these can run side by side and neither
    // reuses the other's network or container name.
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() % 100_000 + line!() as u128
    );
    let network = format!("dc-chaos-{stamp}");
    let store_name = format!("dc-chaos-redis-{stamp}");

    let store = RedisImage::default()
        .with_tag(TAG)
        .with_network(network.clone())
        .with_container_name(store_name.clone())
        .start()
        .expect("Docker is available; `just chaos` needs it");

    let proxy = GenericImage::new("ghcr.io/shopify/toxiproxy", TOXIPROXY)
        .with_exposed_port(8474.tcp())
        .with_exposed_port(LISTEN.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Starting Toxiproxy HTTP server"))
        .with_network(network)
        .start()
        .expect("the proxy image pulls and starts");

    let api = format!(
        "127.0.0.1:{}",
        proxy.get_host_port_ipv4(8474.tcp()).unwrap()
    );
    let listening = proxy.get_host_port_ipv4(LISTEN.tcp()).unwrap();

    // The upstream is named rather than addressed: both containers share a
    // network, so the store answers to its own container name there, and its
    // *published* port has nothing to do with the one the proxy dials.
    post(
        &api,
        "/proxies",
        &format!(
            r#"{{"name":"redis","listen":"0.0.0.0:{LISTEN}","upstream":"{store_name}:6379","enabled":true}}"#
        ),
    );

    let url = format!("redis://127.0.0.1:{listening}");
    let client = redis::Client::open(url.clone()).unwrap();
    let mut connection = client.get_connection().unwrap();

    // Keyspace notifications are off by default, and the watch refuses
    // without them — the same `CONFIG SET` the container suite performs.
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query(&mut connection)
        .unwrap();
    let _: () = connection.set(key, value).unwrap();

    Chaos {
        url,
        api,
        name: "redis".to_owned(),
        _store: store,
        _proxy: proxy,
    }
}

/// Waits for `predicate`, or gives up loudly rather than hanging a runner.
fn wait_until(
    sink: &RemoteSink,
    what: &str,
    predicate: impl Fn(&dynamic_config::RemoteStatus) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline {
        if predicate(&sink.status()) {
            return;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    panic!("timed out waiting for {what}: {:?}", sink.status());
}

/// Writes a new value until the watch announces one, and answers with what
/// it wrote.
///
/// A write that lands before the subscription exists is published to nobody,
/// and a fixed sleep is the flake that costs an afternoon on a loaded runner.
/// Each attempt carries a different host, so nothing can be mistaken for the
/// value that was already serving.
fn deliver_a_change(url: &str, seen: &mpsc::Receiver<()>) -> String {
    let mut connection = redis::Client::open(url).unwrap().get_connection().unwrap();

    for attempt in 1..=10 {
        let host = format!("after-{attempt}");
        let _: () = connection
            .set(
                "myapp/db.json",
                format!(r#"{{"db": {{"host": "{host}"}}}}"#),
            )
            .unwrap();

        if seen.recv_timeout(Duration::from_secs(3)).is_ok() {
            return host;
        }
    }

    panic!("the watch delivered nothing in ten attempts");
}

#[dynamic_config]
#[derive(Debug, serde::Deserialize)]
struct Cut {
    host: String,
}

/// The whole property, in one run: a watch that was delivering, a cable
/// pulled out from under it, and the state an operator is left reading.
#[test]
#[ignore = "chaos: needs Docker and a toxiproxy container — run with `just chaos`"]
fn a_subscription_cut_mid_watch_reports_without_losing_the_last_document() {
    let chaos = chaos_with("myapp/db.json", r#"{"db": {"host": "before"}}"#);

    Cut::set_remote(
        Redis::new(&chaos.url, "myapp/db.json")
            .expect("the URL parses")
            .with_format(dynamic_config::Format::Json),
    );
    Cut::refresh_remote().expect("the store answers the first read");
    // No files: the fetched document is the whole configuration, and
    // initializing through the builder is what installs the snapshot the
    // assertions below read — and what lets the sink reload later.
    Cut::builder("db")
        .init()
        .expect("the fetched document is a configuration");

    assert_eq!(Cut::current().host, "before");

    // Taken after the source is installed, which is what fences it to this
    // generation rather than to a replacement.
    let sink = Cut::remote_sink();
    let delivered = sink.status();

    assert_eq!(delivered.reachable(), Some(true));

    let (changes, seen) = mpsc::channel();
    let source = Redis::new(&chaos.url, "myapp/db.json")
        .expect("the URL parses")
        .with_format(dynamic_config::Format::Json)
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let watcher = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            // The delivery half of the same sink: `apply` records the fetch
            // that would clear the streak, which is what makes the two halves
            // one story rather than two counters.
            let applied = sink.apply(document);

            // Announced *after* the install, so the assertion that follows is
            // reading a snapshot rather than racing one.
            let _ = changes.send(());

            applied
        })
    });

    // A change first, so what is unplugged is a watch that was *working* —
    // written until one arrives, because a write that lands before the
    // subscription exists publishes to nobody and this test would then be
    // asserting about a watch that had never delivered.
    let host = deliver_a_change(&chaos.url, &seen);

    assert_eq!(Cut::current().host, host);

    let serving = sink.status();

    chaos.unplug();

    wait_until(&sink, "the cut subscription to be reported", |status| {
        status.consecutive_failures > 0
    });

    let after = sink.status();

    assert_eq!(
        after.reachable(),
        Some(false),
        "a subscription that was cut is a store that is not answering"
    );
    assert_eq!(
        after.last_fetch, serving.last_fetch,
        "the staleness clock keeps running: a failed attempt is not a fetch, \
         and an alert reads `up == 0` beside *how old* the served document is"
    );
    assert_eq!(
        after.fetches, serving.fetches,
        "and nothing was delivered while the cable was out"
    );
    assert_eq!(
        Cut::current().host,
        host,
        "a store that went away is not a reason to lose the last good \
         configuration"
    );

    // Redis ends the watch on a broken subscription rather than retrying —
    // the loop cannot re-subscribe a connection it no longer has, and a
    // caller that wants one calls `watch` again. What matters is that it
    // ended *loudly*: the failure above is the whole notice anyone gets.
    let ended = watcher.join().expect("the watch thread does not panic");
    let error = ended.expect_err("a cut subscription ends the watch");

    assert!(
        error.to_string().contains("subscription"),
        "the error says what broke: {error}"
    );

    // And the cable going back in is a watch away, on a store that never
    // restarted — which is the half a stopped container could not prove.
    chaos.plug_in();

    let recovered = Redis::new(&chaos.url, "myapp/db.json")
        .expect("the URL parses")
        .with_format(dynamic_config::Format::Json);

    Cut::set_remote(recovered);
    Cut::refresh_remote().expect("the store answers again");
    Cut::builder("db")
        .init()
        .expect("and the document it answers with still installs");

    assert_eq!(Cut::current().host, host);
}
