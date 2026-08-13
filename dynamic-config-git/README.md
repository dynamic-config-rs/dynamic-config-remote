# dynamic-config-git

Read [`dynamic-config`] configuration from a git repository — GitHub, GitLab,
Azure DevOps, Gitea, Bitbucket, or a bare `git@host:repo.git`. One file, or a
set of them read out of one commit.

```toml
[dependencies]
dynamic-config = "0.6.0"
dynamic-config-git = "0.6.0"
```

```rust
use dynamic_config_git::{Credential, GitSource};

AppConfig::set_remote(
    GitSource::builder("https://github.com/acme/config.git")
        .branch("main")
        .path("services/api/config.yaml")
        .credential(Credential::token(std::env::var("GITHUB_TOKEN")?))
        .build()?,
);

// Fetching is explicit; the load that follows touches no network.
AppConfig::refresh_remote()?;
AppConfig::builder("app").init()?;
```

Configuration in git is how a great many teams already work: review, history,
blame and rollback come free, and nobody runs etcd for a file that changes twice
a month.

## Why git rather than five REST APIs

GitHub, GitLab, Azure DevOps, Gitea and Bitbucket all speak git. Their file APIs
are five clients, five auth models and five ways of spelling *this ref*.
"Compatible with all of them" is only reachable through the protocol they share,
and the extra round trip it costs is irrelevant at configuration cadence.

The implementation is [`gix`] — pure Rust, no `libgit2`, no C toolchain and no
OpenSSL question. The exception is SSH, which `gix` carries by spawning the
system `ssh` exactly as `git` does, so **`ssh` must be on the host** for an
`ssh://` remote.

This is the **blocking** `RemoteSource` trait. A git fetch is blocking work —
negotiation, decompression, index writing — and an async program loses nothing:
`refresh_remote_async()` puts a blocking source on `off_thread`.

## What a fetch does

A shallow, single-ref fetch into a bare object database — never a clone, and
never a checkout.

1. Connect and read the ref advertisement. This is what `git ls-remote` costs: a
   few hundred bytes, no objects.
2. If the commit is already in the object database, **stop** — an unchanged ref
   transfers nothing.
3. Otherwise ask for that one commit at depth 1.
4. Read one blob out of the tree, in memory.

**What it costs.** The first fetch transfers the repository's whole tree at that
commit, because that is what the protocol delivers; a monorepo with a gigabyte
of files will transfer a gigabyte once. Later fetches transfer one commit's
worth. This crate is comfortable with a configuration repository and slow to
start against a monorepo.

Filtering by path would cut that first transfer to the files actually read, and
it is not implemented because nothing below this crate can express it: `gix`
0.86 exposes no filter on a fetch — the protocol argument lives one layer down,
on a type only `gix`'s own fetch ever holds — and the filter the large hosts
serve is `blob:none`, which answers with a tree whose blobs are *absent*, so
reading one needs a lazy fetch from a promisor remote that nothing in this
dependency graph implements. Two upstream features away, not one call.

**How long it may take.** `.with_timeout(..)` is thirty seconds by default and
bounds one attempt. What it reaches depends on the transport: with `.tls(..)`
this crate builds the HTTP client, so the number is the connect deadline and the
stall deadline on every read; without it, `gix`'s own transport hardcodes a
twenty-second connect timeout, exposes no other, and a host that accepts the
connection and then says nothing is bounded by nothing. `gix`'s interrupt flag —
which is what bounds the pack — is checked between packets, and a silent host
sends none.

## Which ref

| Constructor | Moves | Reproducible | For |
|---|---|---|---|
| `.branch(..)` — the default, `main` | yes | no | hot reload: a merge *is* the deployment |
| `.tag(..)` | only if force-pushed | nearly | a release train |
| `.commit(..)` | never | yes | pinning a fleet to a known configuration |

A branch is the default because a configuration store's reason to exist is that
the configuration changes; pinning a SHA and then starting a watcher asks a loop
to wait for something that cannot happen.

A SHA is fetched by asking the host for that object directly, which needs
`uploadpack.allowReachableSHA1InWant` — GitHub, GitLab and Azure DevOps allow
it; a self-hosted server may not, and the error says so.

## Authenticating

Every credential can come from a callable, because the ones that matter expire.

| Constructor | Called | For |
|---|---|---|
| `Credential::anonymous()` | — | a public repository |
| `Credential::token(..)` | once | a personal access token, a deploy token, an Azure DevOps PAT |
| `Credential::basic(user, secret)` | once | a host that looks at the user half — a GitLab CI job token |
| `Credential::ssh_agent()` | once | SSH through `SSH_AUTH_SOCK` and `~/.ssh/config` |
| `Credential::ssh_key(path)` | once | SSH with one named key and no other |
| `Credential::ssh_command(..)` | once | a jump host, a vendored client, anything else |
| `Credential::from_fn(..)` | **every fetch** | a token a sidecar rewrites, an environment variable |
| `Credential::expiring(..)` | **when it is about to expire** | a GitHub App installation token, an OIDC-exchanged token |

`Credential::expiring` is the one this crate exists for. A store that takes
`token: String` at construction works in a demo and fails at three in the morning
on the first refresh: an installation token lives one hour and a watcher lives
for the life of the process. The refresh machinery is
[`dynamic-config-store-core`]'s, shared with the Vault, Consul and Firestore
crates — obtained once, reused until a minute before expiry, refreshed under one
lock, and thrown away the moment the host refuses it.

```rust
let credential = Credential::expiring(|_previous| {
    let (token, lives_for) = installation_token()?;   // your GitHub client

    Ok(Issued {
        value: Auth::Https { username: "x-access-token".into(), password: token },
        ttl: Some(lives_for),
    })
});
```

**The JWT-to-installation-token exchange is not in this crate.** Signing an RS256
JWT needs an RSA implementation, and the pure-Rust one carries an unpatched
timing-sidechannel advisory this workspace's licence and advisory gate rejects.
A program that talks to GitHub almost certainly has a client that does the
exchange already; what this crate owes that flow is the *refresh*.

**An SSH key passphrase is not accepted either.** `ssh` has no way to take one
that does not put it on a command line, where `ps` can read it. Use an agent —
`ssh-add` the key once — which is what an agent is for.

## Several files as one document

`.path(..)` takes a path — or a `Keys`, for a set of them. A bare string is
still one file, so nothing that already worked changed.

| | Merged | For |
|---|---|---|
| `.path("conf/app.yaml")` | nothing to merge; handed over byte for byte | one file |
| `.path(Keys::several(["conf/base.yaml", "conf/local.yaml"]))` | **in call order, later wins** | a base and an override, the same rule a list of `.file(..)` calls has |
| `.path(Keys::prefix("conf"))` | **as disjoint sections; an overlap is refused** by name | a directory whose files are the sections of one configuration |

A directory, not a string prefix: a git tree has directories, so
`Keys::prefix("conf")` reads `conf/db.yaml` and does not read `conf-old.yaml`.
The walk is recursive, bounded at 512 files, and every file it finds has to
parse — point it at a directory that holds configuration and nothing else. A
directory has no extension, so it needs `.format(..)`; a list whose members name
two different formats is refused at `build()` rather than parsed as whichever
came first.

**This is the one store here whose multi-file sources can also be watched.**
Everywhere else a watch on a set is refused, because waking on one key and then
re-reading the rest collects a document that never existed at any instant. What
moves here is a *ref*, what a ref names is a *commit*, and every file is read
out of that one commit's tree — so a deployment that writes four files in one
commit is delivered as one document, and there is no interleaving to be had. The
cost is the same one a single-file watch always had: a commit that touches
nothing this source reads still moves the ref, so `on_change` may be called with
an identical document. Spurious, never torn.

## A host this machine does not already trust

An enterprise GitLab behind a private certificate authority, or a host that
wants a client certificate before it will say hello:

```rust
use dynamic_config_git::TlsConfig;

GitSource::builder("https://gitlab.internal/acme/config.git")
    .path(Keys::prefix("conf"))
    .format(Format::Yaml)
    .tls(
        TlsConfig::new()
            .with_ca_certificate_file("/etc/ssl/certs/acme-root.pem")
            .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
    )
    .build()?;
```

`TlsConfig` is [`dynamic-config-store-core`]'s, shared with the other store
crates, so a deployment configuring two stores writes the same calls for both.
The platform's own trust store still applies, so one configuration reaches both
a private host and github.com. There is a bytes spelling of each —
`with_ca_certificate_pem`, `with_client_certificate_pem` — for a program that
already has the material and should not have to put a private key on a disk for
a client to read back.

**Which knob applies to which transport:**

| Reaching | Configured with |
|---|---|
| `https://` | `.tls(..)` — a CA bundle, a client certificate |
| `ssh://`, `git@host:repo.git` | `Credential::ssh_agent()`, `ssh_key(..)`, `ssh_command(..)` — the trust is `known_hosts`, the identity is a key |
| `file://`, a path | neither |

They do not overlap, and a `.tls(..)` on an `ssh://` url is refused at `build()`
rather than quietly doing nothing — a caller who wrote it believes a certificate
authority is what authenticates an SSH host, and it is not.

**There is no way to turn verification off**, and the absence is a decision.
The two situations anybody reaches for that in — a development server with a
self-signed certificate, an enterprise private CA — are both *trusting one more
certificate*, which is one call above and keeps the server authenticated.
Turning verification off does not make TLS weaker in the way a checklist means;
it makes it absent. And git sharpens it: a fetch presents its credential before
it has received anything, so an unverified connection is one that hands a
personal access token to whoever is on the path. There is no name frightening
enough to make that a reasonable thing to offer.

**What `gix` allowed, measured.** `gix` is configured here with the pure-Rust
`reqwest` transport, whose HTTP options type carries `ssl_ca_info` and
`ssl_verify` — and whose `reqwest` backend reads neither; only the `curl`
backend does. Its client is built inside a worker thread from a builder with no
root store, no identity and no hook to reach either, so setting `http.sslCAInfo`
through this crate would do nothing at all. `gix` does take a transport of the
caller's own, and that is what `.tls(..)` installs: `gix` keeps the whole git
half — handshake, protocol version, credential header, packet framing — and only
the client construction is this crate's. Without `.tls(..)` nothing changes and
`gix`'s own transport is used. Adding a C TLS stack to a workspace that has none
was the other way, and it is not worth what this crate's front page claims.

## Where the objects live

A private directory, `0700` from the moment it exists, because it holds the
contents of a private repository. A temporary one by default, removed with the
source; name your own with `.cache_dir(..)` to survive restarts.

Either one would otherwise grow: a shallow fetch of a moving branch writes a
pack every time the branch moves and nothing in git removes it until a `gc`. So
every thirty-second transfer — `.compact_after(n)`, and `0` turns it off — the
object database is emptied and refilled by the fetch that emptied it. The rule
it deletes under is narrow enough for one line: **only what it wrote, only in a
directory it created for itself, and only on a trigger you can see and turn
off.** A `.cache_dir(..)` pointing at a repository that already existed carries
no marker file and is never touched, whatever `compact_after` says.

Two sources in one program may not share a directory: the second is refused at
`build()` rather than allowed to corrupt the first. Two *processes* sharing one
are **not** detected, and this crate will not grow a lock file to change that —
a lock left by a killed process turns a fresh start into a hang, and the
deployment that shares a directory is one volume in two containers, where pid
liveness cannot see across a namespace and two nodes do not agree on a clock.
What is done instead is to bound the damage: concurrent fetches into one object
database are what git's own write-and-rename is built for, and a program that
empties a directory another is reading costs that program **one failed fetch** —
a failure this crate already survives. Give each program its own directory
anyway.

## When a fetch fails

The program stays up. A failed fetch leaves the previously fetched document
installed and the previous configuration serving, and this crate's job is to be
accurate enough for that to work:

- a host that refuses the credential is `ErrorKind::Auth` — waiting will not fix
  a wrong token, and a watch loop should stop rather than hammer;
- everything else is `ErrorKind::Remote`, which a watch loop waits out.

A refused credential that *can* be replaced is refreshed and retried once. One
retry, not a loop: if a fresh credential is refused too, the grant is wrong.

## Untrusted input

Everything the remote sends is untrusted, and each of these is an error rather
than a surprise:

- a **symlink** in the tree is refused, never followed — where it points is the
  repository's choice, not this program's;
- a **directory** or a **submodule** where a file was expected;
- a tree entry, under a `Keys::prefix` walk, whose **name leaves the directory**
  — no porcelain writes one called `..`, but `git mktree` does and so can a pack
  from a host nobody controls, so every discovered path is checked again;
- a blob **over `max_bytes`** (a megabyte by default), refused from the object
  header before any of it is loaded — and `max_bytes` bounds the whole read as
  well as each file in it, so naming a directory does not multiply it by the
  file budget;
- a blob that is **not UTF-8**;
- a configured path with a `.` or `..` component, refused at `build()`.

Nothing is ever checked out, so no name in the tree can name a place outside the
working directory in the first place.

## Watching

git has no watch, so `watch` polls — and says so. Each tick is one ref
advertisement; only a ref that moved costs a transfer. The push half needs
nothing from this crate: whoever terminates a GitHub or GitLab webhook calls
`remote_sink().apply(..)`.

## MSRV

**1.85**, higher than the workspace's 1.71 because `gix` is edition 2024. The
core crate stays where it is; a companion pays for what it pulls in.

## What it deliberately does not do

**Shelling out to the system `git`.** It would reach every credential helper on
the host for free, and it would also be a second implementation of every decision
above. `Credential::ssh_command(..)` reaches the one method the pure-Rust path
cannot, without a second code path.

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-store-core`]: https://docs.rs/dynamic-config-store-core
[`gix`]: https://docs.rs/gix
