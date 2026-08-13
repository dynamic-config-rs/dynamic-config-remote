# dynamic-config-server

An HTTP configuration server for [`dynamic-config`], in the spirit of
Spring Cloud Config Server: one resolved document per **application** and
**profile**, handed to a caller that presented a credential scoped to
that application.

Run it as a service:

```sh
cargo install dynamic-config-server
dynamic-config-server server.toml
```

Or mount its router in a service you already run:

```toml
[dependencies]
dynamic-config-server = "0.6.0"
```

## What it serves

| Endpoint | Returns |
|---|---|
| `GET /{application}/{profile}` | the resolved document — **values**, secrets included |
| `GET /{application}/{profile}/paths` | which keys exist; no values |
| `GET /{application}/{profile}/explain/{path}` | every layer's answer, every value `***` |
| `GET /{application}/{profile}/check` | would the next load succeed; key paths and origins |
| `GET /{application}/{profile}/status` | generation, health, staleness |
| `GET /metrics` | Prometheus text, for the sections this caller may read |
| `GET /healthz`, `GET /readyz` | liveness and readiness. Unauthenticated, and they say nothing else |

Every route is a `GET`. A config server that can be written to is a
different product with a different threat model.

## It is a security boundary

Everything else in this workspace hands configuration to the process that
called it; this crate hands it over a socket. So it **refuses to start**
rather than start permissively — no clients, a token under 32 characters,
an anonymous client without an explicit opt-in, two clients sharing a
token, a private key that anything but its owner can read, or a
non-loopback bind that neither terminates TLS nor acknowledges a
terminator in front are each a refusal naming the key that fixes it.

A credential is scoped to applications rather than to the server, and
"not yours" and "no such thing" are the same 404, the same body and the
same work — so a caller cannot enumerate what it may not read.

## TLS and client certificates

Opt-in twice — the `tls` feature and a `[server.tls]` section — because
a second TLS stack is CVE surface in the one program that holds every
service's secrets, and a deployment with a terminator in front should
carry none of it:

```sh
cargo install dynamic-config-server --features tls
```

```toml
[server.tls]
certificate = "/etc/dynamic-config/server.pem"
key = "/etc/dynamic-config/server.key"
client_ca = "/etc/dynamic-config/clients-ca.pem"   # optional
```

With `client_ca`, **every caller must present a certificate that chains
to it**, verified during the handshake by rustls — a second, independent
factor beside the bearer token. It is a *gate*, not an identity: the
token still names the caller and still scopes what it may read, so a
valid certificate with no token is a 401 and a valid certificate with the
wrong grant is the same 404 as anybody else's.

The private key is the sharpest secret here, and is treated like it: no
error, log, `Debug` or audit line carries a byte of it, and a key file
readable by more than its owner refuses to start with the `chmod` in the
message.

```sh
cargo run -p dynamic-config-server --features tls --example tls_mutual
```

generates a CA, a server certificate and a client certificate, runs the
server over TLS, and shows all three cases end to end.

The threat model, in full, is [in the book](https://ctolon.github.io/dynamic-config/config-server/threat-model.html).

## Documentation

- [The Config Server](https://ctolon.github.io/dynamic-config/config-server.html) — configuration, deployment, and what each endpoint is for
- [`dynamic-config`] — the library it serves

[`dynamic-config`]: https://crates.io/crates/dynamic-config

## License

MIT
