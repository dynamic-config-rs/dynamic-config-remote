# Remote stores

A configuration does not always live in a file next to the binary. It
lives in etcd because a cluster puts it there, in Vault because it is a
secret, in S3 because a deployment writes it once and a hundred pods read
it. This book covers the eight crates that read those, and the server
that hands a section to a program which can reach none of them.

```toml
[dependencies]
dynamic-config = "0.7"
dynamic-config-etcd = "0.7"
```

```rust,ignore
let store = Etcd::new(["http://etcd:2379"]).key("myapp/db.json");

AppConfig::builder()
    .file("config.toml")     // the base, from disk
    .remote(store)           // and what the cluster says on top of it
    .init()?;
```

**Everything a store does is behind one trait.** It answers with a
document — text and a format — and the engine does the rest: the same
layering, the same validation, the same last-known-good cache, the same
`explain`. A store this project has never heard of works the same way, and
[Writing a Store](remote-stores/writing-a-store.md) is that contract.

**Where this fits.** The engine, the macro and the loader are
[dynamic-config](https://dynamic-config-rs.github.io/), with its own book.
The bindings wrap the same engine for
[Python](https://dynamic-config-rs.github.io/python/) and
[Node.js](https://dynamic-config-rs.github.io/node/), and each of them
ships these stores as a second package.

Each crate names the engine with a caret (`"0.7"`), so an engine patch
release reaches a store without the store being re-released — and a
breaking one is picked up here explicitly, in its own time.
