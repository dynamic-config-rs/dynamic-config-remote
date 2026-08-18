# The config server, served and consumed

One `docker compose up` runs the server; one `cargo run` consumes it —
the client half of the pair, which until this example existed only as a
crate nobody drove end to end.

```sh
cd examples/compose
docker compose up -d          # the server, on :8155
cargo run --example served    # a client reading billing/prod from it

# change the document and watch the client follow:
docker compose exec server sh -c \
  'sed -i "s/9000/9001/" /etc/config-server/documents/billing.toml'
```

The server watches its own documents with the same engine it serves them
from — an edit inside the container reaches the client on its next poll.

What this demonstrates, in one screen: the server is a *user* of the
library, the client is an ordinary `RemoteSource`, and everything between
them — bearer token, one application per credential, values only on the
one route that serves them — is the threat model made runnable.
