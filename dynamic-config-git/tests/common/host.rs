//! A scripted git host, on a loopback socket.
//!
//! Speaks just enough HTTP/1.1 for `gix`'s client and delegates the git half to
//! `git upload-pack --stateless-rpc` over a real repository — so the *protocol*
//! is genuine, the same one GitHub serves, and only the parts a test needs to
//! control are ours: which token is currently accepted, and what was asked for.
//!
//! It is generic over the byte stream rather than over `TcpStream`, which is
//! the whole reason it lives here instead of in one test file: `over_http.rs`
//! hands it the socket, and `over_https.rs` hands it the same socket with a
//! `rustls` server connection wrapped around it. Everything above that — the
//! `Authorization` header, the transfer counting, the upload-pack delegation —
//! is one implementation for both.

#![allow(dead_code)] // each test binary uses the part of this it needs

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::Repository;

/// Whatever a request arrives over: a socket, or a socket inside a TLS session.
pub trait Stream: Read + Write {}

impl<T: Read + Write> Stream for T {}

/// How a connection becomes a stream — the identity for plain HTTP, a TLS
/// handshake for HTTPS. `None` means the handshake failed, which is a test
/// asserting that it should.
pub type Wrap = Box<dyn Fn(TcpStream) -> Option<Box<dyn Stream>> + Send + Sync>;

/// What the host currently accepts, and what it has been asked for.
pub struct Host {
    /// The password that works. Changing it is how a test expires a token.
    accepted: Mutex<String>,
    /// Requests that asked for objects, as opposed to listing refs.
    transfers: AtomicUsize,
    /// Every password the client presented, in order.
    presented: Mutex<Vec<String>>,
    /// Times a presented password was refused. An unauthenticated first
    /// request is not one: that is how the client learns a challenge exists.
    refusals: AtomicUsize,
    /// Connections that reached the wrapper, whether or not they got past it.
    /// A TLS test asserts on this: a client that refuses the certificate never
    /// sends a byte of HTTP, so nothing above would notice it tried.
    connections: AtomicUsize,
}

impl Host {
    pub fn new(accepted: &str) -> Arc<Self> {
        Arc::new(Self {
            accepted: Mutex::new(accepted.to_owned()),
            transfers: AtomicUsize::new(0),
            presented: Mutex::new(Vec::new()),
            refusals: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
        })
    }

    pub fn now_accepts(&self, password: &str) {
        *self.accepted.lock().unwrap() = password.to_owned();
    }

    pub fn transfers(&self) -> usize {
        self.transfers.load(Ordering::SeqCst)
    }

    pub fn presented(&self) -> Vec<String> {
        self.presented.lock().unwrap().clone()
    }

    pub fn refusals(&self) -> usize {
        self.refusals.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Serves `repository` until the returned handle is dropped.
///
/// `scheme` is what the url says — `http` or `https` — and `wrap` decides what
/// is actually spoken over the socket.
pub fn serve(
    repository: &Repository,
    host: Arc<Host>,
    scheme: &str,
    wrap: Wrap,
) -> (String, Serving) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let url = format!("{scheme}://{}/config.git", listener.local_addr().unwrap());

    let directory = repository.path().to_owned();
    let running = Arc::new(AtomicUsize::new(1));
    let watching = Arc::clone(&running);

    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if watching.load(Ordering::SeqCst) == 0 {
                return;
            }

            let Ok(stream) = stream else { return };

            host.connections.fetch_add(1, Ordering::SeqCst);

            // One request per connection: `Connection: close` on every
            // response, so there is no keep-alive state machine to get wrong.
            if let Some(stream) = wrap(stream) {
                let _ = answer(stream, &directory, &host);
            }
        }
    });

    (
        url,
        Serving {
            running,
            server: Some(server),
        },
    )
}

pub struct Serving {
    running: Arc<AtomicUsize>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.running.store(0, Ordering::SeqCst);

        // Unblocks `incoming()`, which is parked in `accept`.
        drop(self.server.take());
    }
}

/// The smart-HTTP protocol, in the two requests it actually consists of.
fn answer(
    stream: Box<dyn Stream>,
    directory: &std::path::Path,
    host: &Host,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    let mut request = String::new();
    reader.read_line(&mut request)?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();

        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }

        headers.push(line.trim_end().to_owned());
    }

    let header = |name: &str| {
        headers.iter().find_map(|line| {
            let (key, value) = line.split_once(':')?;

            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };

    // Authorization first: a host that leaks the refs to an unauthenticated
    // client is not testing anything.
    let presented = header("authorization").and_then(|value| {
        let encoded = value.strip_prefix("Basic ")?;
        let decoded = base64(encoded)?;
        let (_user, password) = decoded.split_once(':')?;

        Some(password.to_owned())
    });

    if let Some(password) = &presented {
        host.presented.lock().unwrap().push(password.clone());
    }

    if presented.as_deref() != Some(host.accepted.lock().unwrap().as_str()) {
        if presented.is_some() {
            host.refusals.fetch_add(1, Ordering::SeqCst);
        }

        return respond(
            &mut reader,
            "401 Unauthorized",
            "text/plain",
            b"bad credentials",
            Some("Basic realm=\"git\""),
        );
    }

    let protocol = header("git-protocol").unwrap_or_default();

    if request.starts_with("GET ") {
        let mut body = pkt_line("# service=git-upload-pack\n");
        body.extend_from_slice(b"0000");
        body.extend_from_slice(&upload_pack(directory, &protocol, true, &[])?);

        return respond(
            &mut reader,
            "200 OK",
            "application/x-git-upload-pack-advertisement",
            &body,
            None,
        );
    }

    let payload = read_body(
        &mut reader,
        &header("content-length"),
        &header("transfer-encoding"),
    )?;

    // A fetch asks for objects; `ls-refs` and the v1 capability round do not.
    // Counting the difference is how "an unchanged ref transfers nothing"
    // becomes an assertion rather than a claim.
    let asked_for_objects = payload.windows(13).any(|window| window == b"command=fetch")
        || payload.windows(5).any(|window| window == b"want ");

    if asked_for_objects {
        host.transfers.fetch_add(1, Ordering::SeqCst);
    }

    let body = upload_pack(directory, &protocol, false, &payload)?;

    respond(
        &mut reader,
        "200 OK",
        "application/x-git-upload-pack-result",
        &body,
        None,
    )
}

/// Runs the real `git upload-pack`, which is what makes this a git host rather
/// than a fixture.
fn upload_pack(
    directory: &std::path::Path,
    protocol: &str,
    advertise: bool,
    input: &[u8],
) -> std::io::Result<Vec<u8>> {
    let mut command = Command::new("git");

    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .arg("upload-pack")
        .arg("--stateless-rpc");

    if advertise {
        command.arg("--advertise-refs");
    }

    if !protocol.is_empty() {
        command.env("GIT_PROTOCOL", protocol);
    }

    let mut child = command
        .arg(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    child.stdin.take().expect("a pipe").write_all(input)?;

    let output = child.wait_with_output()?;

    Ok(output.stdout)
}

fn respond(
    reader: &mut BufReader<Box<dyn Stream>>,
    status: &str,
    content_type: &str,
    body: &[u8],
    authenticate: Option<&str>,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        body.len()
    );

    if let Some(challenge) = authenticate {
        head.push_str(&format!("WWW-Authenticate: {challenge}\r\n"));
    }

    head.push_str("\r\n");

    let stream = reader.get_mut();

    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The request body, however the client chose to frame it.
///
/// `gix`'s HTTP transport streams the upload-pack request, so it may arrive
/// chunked rather than with a `Content-Length` — which a mock that only handles
/// the latter would read as an empty request and answer nonsense to. The
/// transport in `src/tls.rs` sends a bounded body and so uses the other branch;
/// both are exercised, one per test file.
fn read_body(
    reader: &mut BufReader<Box<dyn Stream>>,
    content_length: &Option<String>,
    transfer_encoding: &Option<String>,
) -> std::io::Result<Vec<u8>> {
    if let Some(length) = content_length.as_ref().and_then(|value| value.parse().ok()) {
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;

        return Ok(body);
    }

    if !transfer_encoding
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        return Ok(Vec::new());
    }

    let mut body = Vec::new();

    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;

        let size = usize::from_str_radix(header.trim(), 16).unwrap_or(0);

        if size == 0 {
            // The trailer and the final CRLF; nothing here reads trailers.
            let mut rest = String::new();
            let _ = reader.read_line(&mut rest);

            return Ok(body);
        }

        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);

        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

fn pkt_line(text: &str) -> Vec<u8> {
    format!("{:04x}{text}", text.len() + 4).into_bytes()
}

/// Just enough base64 to read an `Authorization: Basic` header.
fn base64(encoded: &str) -> Option<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut bits = 0u32;
    let mut held = 0;
    let mut bytes = Vec::new();

    for byte in encoded.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;

        bits = (bits << 6) | value;
        held += 6;

        if held >= 8 {
            held -= 8;
            bytes.push((bits >> held) as u8);
        }
    }

    String::from_utf8(bytes).ok()
}

/// The plain-socket wrapper: HTTP with nothing around it.
#[must_use]
pub fn plain() -> Wrap {
    Box::new(|stream| Some(Box::new(stream)))
}
