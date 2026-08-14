//! Unplugging etcd while a watch stream is open.
//!
//! ```text
//! just chaos
//! ```
//!
//! `#[ignore]`d, like every chaos test: two containers and tens of seconds
//! belong to a nightly and a release gate rather than to every commit.
//! `cargo test -p dynamic-config-etcd` still compiles this, which is what
//! keeps it from rotting.
//!
//! # What only a real stream can show
//!
//! The unit test in `src/lib.rs` proves a watch that was never established
//! reports. This proves the other one — a stream that *was* delivering and
//! then stopped, which is the failure an operator actually meets and the
//! only one where "the last delivery is old" and "the store is down" are two
//! different facts.
//!
//! A proxy rather than a stopped container, for the reason written out in
//! `dynamic-config-redis/tests/chaos.rs`: a restarted container comes back
//! on a different host port.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use dynamic_config::{dynamic_config, Format, RemoteSink};
use dynamic_config_etcd::Etcd;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const IMAGE: &str = "quay.io/coreos/etcd";
const TAG: &str = "v3.5.17";
const TOXIPROXY: &str = "2.12.0";
/// The port the proxy listens on *inside* its container.
const LISTEN: u16 = 8666;

struct Chaos {
    endpoint: String,
    api: String,
    _store: ContainerAsync<GenericImage>,
    _proxy: ContainerAsync<GenericImage>,
}

impl Chaos {
    fn unplug(&self) {
        post(&self.api, "/proxies/etcd", r#"{"enabled":false}"#);
    }

    fn plug_in(&self) {
        post(&self.api, "/proxies/etcd", r#"{"enabled":true}"#);
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

/// etcd behind a proxy, with `key` already written into it.
async fn chaos_with(key: &str, value: &str) -> Chaos {
    let stamp = format!("{}-{}", std::process::id(), line!());
    let network = format!("dc-chaos-{stamp}");
    let store_name = format!("dc-chaos-etcd-{stamp}");

    let store = GenericImage::new(IMAGE, TAG)
        .with_exposed_port(2379.tcp())
        .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
        .with_cmd([
            "etcd",
            "--advertise-client-urls=http://0.0.0.0:2379",
            "--listen-client-urls=http://0.0.0.0:2379",
        ])
        .with_network(network.clone())
        .with_container_name(store_name.clone())
        .start()
        .await
        .expect("Docker is available; `just chaos` needs it");

    let proxy = GenericImage::new("ghcr.io/shopify/toxiproxy", TOXIPROXY)
        .with_exposed_port(8474.tcp())
        .with_exposed_port(LISTEN.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Starting Toxiproxy HTTP server"))
        .with_network(network)
        .start()
        .await
        .expect("the proxy image pulls and starts");

    let api = format!(
        "127.0.0.1:{}",
        proxy.get_host_port_ipv4(8474.tcp()).await.unwrap()
    );
    let listening = proxy.get_host_port_ipv4(LISTEN.tcp()).await.unwrap();

    post(
        &api,
        "/proxies",
        &format!(
            r#"{{"name":"etcd","listen":"0.0.0.0:{LISTEN}","upstream":"{store_name}:2379","enabled":true}}"#
        ),
    );

    let endpoint = format!("http://127.0.0.1:{listening}");

    put(&endpoint, key, value).await;

    Chaos {
        endpoint,
        api,
        _store: store,
        _proxy: proxy,
    }
}

async fn put(endpoint: &str, key: &str, value: &str) {
    let mut client = etcd_client::Client::connect([endpoint], None)
        .await
        .expect("the proxy carries a connection through");

    client.put(key, value, None).await.expect("the write lands");
}

/// Waits for `predicate`, or gives up loudly rather than hanging a runner.
async fn wait_until(
    sink: &RemoteSink,
    what: &str,
    predicate: impl Fn(&dynamic_config::RemoteStatus) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);

    while Instant::now() < deadline {
        if predicate(&sink.status()) {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("timed out waiting for {what}: {:?}", sink.status());
}

/// Writes a new value until the watch announces one, and answers with what
/// it wrote.
///
/// A write that lands before the stream is established belongs to a revision
/// the watch starts *after*, and a fixed sleep is the flake that costs an
/// afternoon on a loaded runner.
async fn deliver_a_change(
    endpoint: &str,
    seen: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> String {
    for attempt in 1..=10 {
        let host = format!("after-{attempt}");

        put(
            endpoint,
            "myapp/db.json",
            &format!(r#"{{"db": {{"host": "{host}"}}}}"#),
        )
        .await;

        if tokio::time::timeout(Duration::from_secs(3), seen.recv())
            .await
            .is_ok()
        {
            return host;
        }
    }

    panic!("the watch delivered nothing in ten attempts");
}

#[dynamic_config]
#[derive(Debug, serde::Deserialize)]
struct Streamed {
    host: String,
}

/// The whole property, in one run: a stream that was delivering, a cable
/// pulled out from under it, and the state an operator is left reading.
#[tokio::test]
#[ignore = "chaos: needs Docker and a toxiproxy container — run with `just chaos`"]
async fn a_stream_cut_mid_watch_reports_without_losing_the_last_document() {
    let chaos = chaos_with("myapp/db.json", r#"{"db": {"host": "before"}}"#).await;

    Streamed::set_remote_async(
        Etcd::new([chaos.endpoint.as_str()], "myapp/db.json")
            .await
            .expect("the endpoint parses")
            .with_format(Format::Json),
    );
    Streamed::refresh_remote_async()
        .await
        .expect("the store answers the first read");
    // No files: the fetched document is the whole configuration.
    Streamed::builder("db")
        .init()
        .expect("the fetched document is a configuration");

    assert_eq!(Streamed::current().host, "before");

    let sink = Streamed::remote_sink();

    assert_eq!(sink.status().reachable(), Some(true));

    // A tokio channel rather than the standard one: a blocking `recv` on
    // this test's own thread would starve the watch task it is waiting for.
    let (changes, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let source = Etcd::new([chaos.endpoint.as_str()], "myapp/db.json")
        .await
        .expect("the endpoint parses")
        .with_format(Format::Json)
        .reporting_to(sink);

    let watcher = tokio::spawn(async move {
        source
            .watch(move |document| {
                let applied = sink.apply(document);

                // Announced after the install, so an assertion reading the
                // snapshot is not racing one.
                let _ = changes.send(());

                applied
            })
            .await
    });

    // A change first, so what is unplugged is a watch that was *working* —
    // written until one arrives, because a write that lands before the
    // stream is established is a revision this watch starts after.
    let host = deliver_a_change(&chaos.endpoint, &mut seen).await;

    assert_eq!(Streamed::current().host, host);

    let delivered = sink.status();

    chaos.unplug();

    wait_until(&sink, "the cut stream to be reported", |status| {
        status.consecutive_failures > 0
    })
    .await;

    let cut = sink.status();

    assert_eq!(
        cut.reachable(),
        Some(false),
        "a stream that was cut is a store that is not answering"
    );
    assert_eq!(
        cut.last_fetch, delivered.last_fetch,
        "the staleness clock keeps running: a failed attempt is not a fetch, \
         and an alert reads `up == 0` beside *how old* the served document is"
    );
    assert_eq!(
        Streamed::current().host,
        host,
        "a store that went away is not a reason to lose the last good \
         configuration"
    );

    // etcd's loop ends on a stream it cannot re-establish rather than
    // retrying forever — and it ends *loudly*, which is the whole notice a
    // caller that spawned it and dropped the handle will ever get.
    let ended = tokio::time::timeout(Duration::from_secs(60), watcher)
        .await
        .expect("the watch ends rather than hanging")
        .expect("the task does not panic");

    ended.expect_err("a cut stream ends the watch");

    // And with the cable back in, the store is the same store: nothing
    // restarted, and the document is still there to be read.
    chaos.plug_in();

    Streamed::set_remote_async(
        Etcd::new([chaos.endpoint.as_str()], "myapp/db.json")
            .await
            .expect("the endpoint parses")
            .with_format(Format::Json),
    );
    Streamed::refresh_remote_async()
        .await
        .expect("the store answers again");
    Streamed::builder("db")
        .init()
        .expect("and the document it answers with still installs");

    assert_eq!(Streamed::current().host, host);
}
