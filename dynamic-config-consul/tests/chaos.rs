//! Unplugging the agent while a blocking query is waiting on it.
//!
//! ```text
//! just chaos
//! ```
//!
//! `#[ignore]`d, like every chaos test: two containers and tens of seconds
//! belong to a nightly and a release gate rather than to every commit.
//! `cargo test -p dynamic-config-consul` still compiles this, which is what
//! keeps it from rotting.
//!
//! # The half no other suite can show
//!
//! Consul's loop **survives** a failure — it records it, waits, and queries
//! again — so it is the one store where "the cable went back in" has an
//! ending worth asserting: the streak clears, the document that arrives is
//! delivered, and nobody had to call anything. `mock_agent.rs` proves the
//! reporting with a scripted 500; what it cannot do is put the agent back.
//!
//! A proxy rather than a stopped container, for the reason written out in
//! `dynamic-config-redis/tests/chaos.rs`: a restarted container comes back
//! on a different host port, and a source pointing at nothing cannot prove
//! a recovery.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dynamic_config::{dynamic_config, Format, RemoteSink, RemoteWatch};
use dynamic_config_consul::Consul;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use testcontainers_modules::consul::Consul as ConsulImage;

const TOXIPROXY: &str = "2.12.0";
/// The port the proxy listens on *inside* its container.
const LISTEN: u16 = 8666;

struct Chaos {
    address: String,
    api: String,
    _store: Container<ConsulImage>,
    _proxy: Container<GenericImage>,
}

impl Chaos {
    fn unplug(&self) {
        self.control(r#"{"enabled":false}"#);
    }

    fn plug_in(&self) {
        self.control(r#"{"enabled":true}"#);
    }

    fn control(&self, body: &str) {
        post(&self.api, "/proxies/consul", body);
    }
}

/// One HTTP POST, written by hand — see the note in the Redis chaos suite.
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

/// Consul behind a proxy, with `key` already written into it.
fn chaos_with(key: &str, value: &str) -> Chaos {
    let stamp = format!("{}-{}", std::process::id(), line!());
    let network = format!("dc-chaos-{stamp}");
    let store_name = format!("dc-chaos-consul-{stamp}");

    let store = ConsulImage::default()
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

    post(
        &api,
        "/proxies",
        &format!(
            r#"{{"name":"consul","listen":"0.0.0.0:{LISTEN}","upstream":"{store_name}:8500","enabled":true}}"#
        ),
    );

    let address = format!("http://127.0.0.1:{listening}");

    write(&address, key, value);

    Chaos {
        address,
        api,
        _store: store,
        _proxy: proxy,
    }
}

fn write(address: &str, key: &str, value: &str) {
    let response = ureq::put(&format!("{address}/v1/kv/{key}"))
        .send(value)
        .expect("the agent takes the write");

    assert!(response.status().is_success(), "{}", response.status());
}

/// Waits for `predicate`, or gives up loudly rather than hanging a runner.
fn wait_until(
    sink: &RemoteSink,
    what: &str,
    predicate: impl Fn(&dynamic_config::RemoteStatus) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);

    while Instant::now() < deadline {
        if predicate(&sink.status()) {
            return;
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    panic!("timed out waiting for {what}: {:?}", sink.status());
}

/// Writes a new value until the watch announces one, and answers with what
/// it wrote.
///
/// Every attempt carries a different host, because a document identical to
/// the last delivered one is suppressed by the loop — so retrying with the
/// same text would prove nothing and hang.
fn deliver_a_change(address: &str, seen: &mpsc::Receiver<()>) -> String {
    for attempt in 1..=10 {
        let host = format!("after-{attempt}");

        write(
            address,
            "myapp/db.json",
            &format!(r#"{{"db": {{"host": "{host}"}}}}"#),
        );

        if seen.recv_timeout(Duration::from_secs(6)).is_ok() {
            return host;
        }
    }

    panic!("the watch delivered nothing in ten attempts");
}

#[dynamic_config]
#[derive(Debug, serde::Deserialize)]
struct Blocked {
    host: String,
}

/// The whole property: a working watch, a cable pulled out, the state an
/// operator reads — and then the cable back in, with nobody restarting
/// anything.
#[test]
#[ignore = "chaos: needs Docker and a toxiproxy container — run with `just chaos`"]
fn a_blocking_query_cut_mid_watch_reports_and_recovers_on_its_own() {
    let chaos = chaos_with("myapp/db.json", r#"{"db": {"host": "before"}}"#);

    Blocked::set_remote(Consul::new(&chaos.address, "myapp/db.json").with_format(Format::Json));
    Blocked::refresh_remote().expect("the agent answers the first read");
    // No files: the fetched document is the whole configuration.
    Blocked::builder("db")
        .init()
        .expect("the fetched document is a configuration");

    assert_eq!(Blocked::current().host, "before");

    let sink = Blocked::remote_sink();
    let serving = sink.status();

    assert_eq!(serving.reachable(), Some(true));

    let (changes, seen) = mpsc::channel();
    let source = Consul::new(&chaos.address, "myapp/db.json")
        .with_format(Format::Json)
        // The blocking query's wait, kept short: the loop has to notice the
        // cut, and a five-minute wait would be five minutes of this test
        // sitting in a socket that is not going to answer.
        .with_wait(Duration::from_secs(2))
        .with_timeout(Duration::from_secs(2))
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let watcher = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let applied = sink.apply(document);

            // Announced after the install, so an assertion reading the
            // snapshot is not racing one.
            let _ = changes.send(());

            applied
        })
    });

    // Written until one arrives, and each with a *different* value.
    //
    // The first query a blocking watch makes carries index 0, which Consul
    // answers immediately with whatever is stored — the priming read, which
    // delivers nothing by design. A write that lands inside that window is
    // folded into the priming answer and never announced, which is a race
    // this test lost about one run in two. Re-writing the same value would
    // not help either: the loop suppresses an unchanged document.
    let host = deliver_a_change(&chaos.address, &seen);

    assert_eq!(Blocked::current().host, host);

    let delivered = sink.status();

    chaos.unplug();

    wait_until(&sink, "the cut query to be reported", |status| {
        status.consecutive_failures > 0
    });

    let cut = sink.status();

    assert_eq!(
        cut.reachable(),
        Some(false),
        "a query that cannot reach the agent is a store that is not answering"
    );
    assert_eq!(
        cut.last_fetch, delivered.last_fetch,
        "the staleness clock keeps running: a failed attempt is not a fetch"
    );
    assert!(
        !watcher.is_finished(),
        "this loop survives a failure — which is exactly why reporting it is \
         the only way anyone hears about it"
    );
    assert_eq!(
        Blocked::current().host,
        host,
        "an agent that went away is not a reason to lose the last good \
         configuration"
    );

    // Back in, with nothing restarted and nobody called. Written the same
    // way: the loop is mid-sleep, so the first write may land before it
    // queries again.
    chaos.plug_in();

    let recovered = deliver_a_change(&chaos.address, &seen);

    let healed = sink.status();

    assert_eq!(
        healed.reachable(),
        Some(true),
        "a delivery is what clears the streak — nothing else does"
    );
    assert_eq!(healed.consecutive_failures, 0);
    assert!(healed.last_fetch > delivered.last_fetch);
    assert_eq!(Blocked::current().host, recovered);

    watch.stop();

    watcher
        .join()
        .expect("the watch thread does not panic")
        .expect("and a stopped watch is not a failure");
}
