# Git

[`dynamic-config-git`](https://docs.rs/dynamic-config-git) reads configuration
from a git repository — GitHub, GitLab, Azure DevOps, Gitea, Bitbucket, or a
bare `git@host:repo.git` behind a VPN — over the **blocking** `RemoteSource`.
One file, or a set of them read out of one commit.

```toml
[dependencies]
dynamic-config = "<version>"
dynamic-config-git = "<version>"
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
AppConfig::refresh_remote()?;
```

## Why one crate covers every host

Every one of those services speaks git. Their file APIs do not speak anything
in common: five clients, five auth models, five pagination stories and five
ways of spelling *this ref*. "Compatible with all of them" is only reachable
through the protocol they share, and the extra round trip that protocol costs
is irrelevant for a file that changes twice a month.

The implementation is [`gix`](https://docs.rs/gix): pure Rust, so this crate
adds no C toolchain and no OpenSSL question to a workspace that has neither.
The one exception is SSH, which `gix` carries by spawning the system `ssh`
exactly as `git` does — so an `ssh://` remote needs `ssh` on the host, and in
exchange everything already configured for it works: `~/.ssh/config`,
`known_hosts`, a `ProxyJump`, a hardware key.

## What a fetch is

**A shallow, single-ref fetch into a bare object database — never a clone, and
never a checkout.** Reading the ref advertisement is what `git ls-remote`
costs: a few hundred bytes and no objects. If the commit it names is already in
the object database the fetch stops there, so **an unchanged ref transfers
nothing** — which is the only reason polling a git host on a timer is a
defensible thing to do. Otherwise the commit is fetched at depth 1 and one blob
is read out of its tree, in memory.

The cost worth knowing: the *first* fetch transfers the repository's whole tree
at that commit, because a commit's tree is what the protocol delivers. A
configuration repository is a few hundred kilobytes; a monorepo is a monorepo.

Filtering by path would cut that down, and it is not implemented — not as a
to-do, but because nothing below this crate can express it. `gix` 0.86 exposes
no filter on a fetch at all: the protocol argument exists one layer down, in
`gix-protocol`, on a type that only `gix`'s own fetch ever holds, so reaching it
means driving the protocol directly and re-implementing pack writing, the
shallow boundary and ref updating — a second copy of every decision on this
page. And the filter the large hosts actually serve is `blob:none`, whose answer
is a tree with the blobs *missing*; reading one then requires a lazy fetch from
a promisor remote, which nothing in this dependency graph implements. So it is
two upstream features away rather than one call, and until they arrive this
store is comfortable with a configuration repository and slow to start against
a monorepo.

## How long a fetch may take

`.with_timeout(..)` is thirty seconds by default and bounds one attempt. What it
reaches depends on which transport the source uses, which is worth stating
plainly rather than promising a number twice:

| Phase | With `.tls(..)` — this crate's client | Without — `gix`'s own |
|---|---|---|
| connecting | this number | twenty seconds, `gix`'s, not configurable |
| the handshake and the ref advertisement | this number, per read | unbounded |
| negotiation and the pack | this number, per read | this number |

`gix` bounds a fetch with an interrupt flag it checks *between packets*, which
is a real deadline for the part that transfers data and no deadline at all for a
host that accepts the connection and then sends nothing — there are no packets
for the check to be between. Its `reqwest` transport hardcodes its connect
timeout and reads none of the timeout settings its own options type carries, so
that column is not this crate's to close from outside. It could have been closed
by routing every `https://` source through the client `.tls(..)` builds; that
would have cost redirect following and `http.extraHeader` for every caller,
including the ones whose host answers in milliseconds, to work around one field
`gix` does not read. Saying which transport bounds what is the better trade.

## Which ref, and why a branch is the default

| | Moves | Reproducible | For |
|---|---|---|---|
| `.branch(..)` — the default, `main` | yes | no | hot reload: a merge to `main` *is* the deployment |
| `.tag(..)` | only if force-pushed | nearly | a release train |
| `.commit(..)` | never | yes | pinning a fleet to a known configuration |

Both ends are legitimate, and the default goes to the moving one because a
configuration store's reason to exist is that the configuration changes:
pinning a SHA and then starting a watcher asks a loop to wait for something
that cannot happen. Pin the SHA when reproducibility matters more than reload —
and note that a SHA is fetched by asking the host for that object directly,
which needs `uploadpack.allowReachableSHA1InWant`. GitHub, GitLab and Azure
DevOps allow it; a self-hosted server may not, and the error says so.

## The credential is a function

This is the decision the crate exists for. A store that takes `token: String`
at construction works in a demo and fails at three in the morning on the first
refresh: a GitHub App installation token lives one hour, a workload-identity
token lives minutes, and a watcher lives for the life of the process.

| Constructor | Called | For |
|---|---|---|
| `Credential::anonymous()` | — | a public repository |
| `Credential::token(..)`, `basic(..)`, `ssh_agent()`, `ssh_key(..)`, `ssh_command(..)` | once | a value that cannot change |
| `Credential::from_fn(..)` | every fetch | a token a sidecar rewrites; an environment variable |
| `Credential::expiring(..)` | when it is about to expire | an App installation token; an OIDC-exchanged token |

`expiring` hands its closure to the same cache Vault, Consul and Firestore use:
obtained once, reused until a minute before the expiry the issuer reported,
refreshed under one lock so eight threads produce one exchange, and thrown away
the moment the host refuses it so the next attempt obtains a fresh one. One
replacement and one retry, never a loop — a second refusal means the grant is
wrong, and retrying would turn a clear failure into a hang.

Two things it deliberately does **not** do, because doing them badly would be
worse than not doing them:

- **The GitHub App JWT-to-token exchange is not here.** It needs an RS256
  signature, and the pure-Rust RSA implementation carries an unpatched
  timing-sidechannel advisory this project's dependency gate rejects. A program
  that talks to GitHub has a client that does the exchange; what this crate
  owes that flow is the refresh, and that is `expiring`.
- **An SSH key passphrase is not accepted.** `ssh` has no way to take one that
  does not put it on a command line where `ps` can read it. Use an agent —
  `ssh-add` the key once, which is what an agent is for.

## Several files, and why this store can watch them

`.path(..)` takes a path, or a `Keys` for a set. A bare string is still one
file.

| | Merged | For |
|---|---|---|
| `.path("conf/app.yaml")` | nothing to merge; handed over byte for byte | one file |
| `.path(Keys::several([a, b]))` | in call order, **later wins** | a base and an override |
| `.path(Keys::prefix("conf"))` | as disjoint sections; an **overlap is refused** | a directory whose files are the sections |

The two rules differ on purpose, and it is the same distinction every store in
this family makes: a caller who wrote the list wrote the precedence with it,
while the order a git tree lists its entries in is nobody's decision, so two
files under one directory supplying the same path is a deployment bug and is
reported as one.

A **directory**, not a string prefix — a git tree has directories, so
`Keys::prefix("conf")` reads `conf/db.yaml` and does not read `conf-old.yaml`.
That is the one place this store's prefix means something different from a
key-value store's, and it means it because the underlying thing is different.
The walk is recursive and bounded at 512 files; every file it finds has to
parse, so point it at a directory that holds configuration and nothing else. A
directory has no extension, so `.format(..)` is required.

**A multi-file source here can be watched**, and everywhere else in this family
it cannot. The reason recorded for the refusal elsewhere is that a set cannot be
re-read as of one instant: waking on a change to `myapp/db` and re-reading key
by key collects the new `myapp/db` and whatever `myapp/server` happens to be
halfway through a deployment — a document that never existed, installed and
served until the next change.

Neither half of that survives contact with git. What moves is a **ref**; what a
ref names is a **commit**; a commit has one tree, and every file is read out of
it. A deployment that writes four files in one commit is delivered as one
document. A deployment that writes them in four commits is delivered as up to
four documents, each of which is a state the repository really was in. The
atomicity is a property of the object model, not something the crate arranges,
which is why it costs nothing and needs no second code path.

The cost runs the other way, and it is the cost a single-file watch always had:
a commit touching nothing this source reads still moves the ref, so `on_change`
may be handed a document identical to the last. Spurious, never torn — the
loader diffs it and reports no changes.

## Reaching a host this machine does not trust

`.tls(..)` takes the same `TlsConfig` every other store crate takes: a CA
bundle, from a file or from bytes, and a client certificate for mTLS. The
platform's own trust store still applies, so one source configuration reaches
both a private GitLab and github.com.

```rust
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

**It is the `https://` knob and only that one.** An `ssh://` remote
authenticates its host through `known_hosts` and its client through a key —
`Credential::ssh_agent()`, `ssh_key(..)` or `ssh_command(..)` — and a `.tls(..)`
on one is refused at `build()` rather than quietly ignored, because a caller who
wrote it believes something about SSH that is not true.

**There is no way to turn verification off.** The two situations people reach
for it in are a development server with a self-signed certificate and an
enterprise private CA, and both are *trusting one more certificate*, which is
one call above and leaves the server authenticated. Turning verification off
does not make TLS weaker in the way a checklist means; it makes it absent. git
gives that a second edge: a fetch presents its credential before it has received
anything, so an unverified connection is one that hands a token to whoever is on
the path.

What this cost is worth knowing, because it is unusual. `gix`'s HTTP options
carry `ssl_ca_info` and `ssl_verify`, and its pure-Rust `reqwest` backend reads
neither — only the `curl` backend does, and its client is built with no root
store, no identity and no hook to reach either. So configuring `http.sslCAInfo`
here would have done nothing, and a store that silently ignores "trust this
authority" is a program that believes it is pinned and is not. `gix` does accept
a transport of the caller's own, so `.tls(..)` installs one: `gix` keeps the
entire git half and only the HTTP client is this crate's. Without `.tls(..)`,
`gix`'s own transport is used unchanged. The alternative was a C TLS stack in a
workspace that has no C dependency at all, which would have cost more than the
feature is worth.

One behaviour differs on that client, and a caller reaching an unusual host
should know it: **it follows no redirect.** The reason is not the obvious one —
`reqwest` removes `Authorization` itself when a redirect crosses to another
host, port or scheme, so the credential would not travel. It is that a git fetch
is two requests against one base url, a `GET` of the advertisement and a `POST`
of the negotiation, and following a redirect on the first leaves the second
addressed to the old url; a `301` on a `POST` is turned into a bodyless `GET` by
every client that obeys the specification. Making it work means rewriting the
base url for the rest of the conversation and deciding whether the identity may
be reused at the new address — security-relevant code, and not worth a second
copy for a transport only reached by callers who named an unusual host. Such a
host is named by its final url instead, and the error says exactly that.

## What is on disk, and who can read it

A directory holding the objects of a private repository, created `0700` **by
the call that creates it** rather than `chmod`ed afterwards: a `chmod` after the
fact leaves a window in which it is world-readable, and a window is all an
attacker on a shared host needs.

By default it is a temporary directory, removed with the source — nothing
survives the process, which is the right default for a container and costs one
full fetch per start. `.cache_dir(..)` names your own instead, so a restart
transfers almost nothing.

Either one would otherwise grow without bound: a shallow fetch of a moving
branch writes a pack every time the branch moves, the old ones stop being
reachable the moment the local ref moves, and nothing in git removes them until
a `gc`. A watcher left running for a month is the case that matters. So the
store compacts — every thirty-second transfer by default, `.compact_after(n)`,
and `0` turns it off for a deployment that would rather run `git gc
--prune=now` on its own cadence — by emptying the object database and letting
the fetch that emptied it refill it. One full transfer per thirty-two pushes, a
bounded fraction of what was going to transfer anyway, and it is not a `gc`
because a store that rewrites an object database is a store that can corrupt
one; this one can only lose a copy of something the remote still has.

A store that deletes needs a rule narrow enough to state in a line: **only what
it wrote, only in a directory it created for itself, and only on a trigger the
caller can see and turn off.** A marker file is written when this crate
initialises the database, and a `.cache_dir(..)` pointing at a repository that
already existed does not have one and is never touched, whatever
`compact_after` says.

Two sources in one program may not share a directory, and the second one is
refused at `build()` rather than allowed to corrupt the first. Two **processes**
sharing one are not detected, and this store will not grow a lock file to change
that. The obvious answer is the wrong one here: the deployment that shares a
working directory is not two programs on one host, it is one volume mounted into
two containers, where pid liveness cannot see across a pid namespace — both
containers have a live pid 1 — and an age bound is a guess about how long a
fetch takes, measured against a clock two nodes do not share. An advisory
`flock` has no staleness and no reach either, being unreliable on exactly the
network filesystems that make the sharing possible. What is done instead is to
bound the damage: concurrent fetches into one object database are what git's
write-and-rename is built for, and a program that empties a directory another is
reading costs that program one failed fetch — which leaves the previous document
installed and a watch waiting out one interval. Give each program its own
directory anyway.

## Untrusted input

Everything a remote repository sends is untrusted, and the defence is
structural: **nothing is ever checked out**, so no name in the tree can name a
place outside the working directory. On top of that, each of these is an error
rather than a surprise — a symlink (refused, never followed: where it points is
the repository's choice and not this program's), a directory, a submodule, a
blob over `max_bytes` (refused from the object header, before any of it is
loaded) and a blob that is not UTF-8. `max_bytes` bounds the **whole read** as
well as each file in it: a megabyte is what a caller who says a megabyte is
offering this source, and naming a directory rather than a file does not
multiply it by the 512-file budget. The two limits stay because they bound
different things — a tree of a hundred thousand empty files costs the walk
rather than the memory. A configured path with a `.` or `..`
component is refused at `build()`, where the mistake was made — and so is a
tree entry whose *own* name leaves the directory, which no porcelain writes but
`git mktree` will and a hostile pack can.

## Failing without taking the program down

A refused credential is `ErrorKind::Auth` and everything else is
`ErrorKind::Remote` — the distinction a reload loop acts on, because the second
may fix itself while the loop waits and the first will not. A `watch` whose
credential is a constant the host refuses ends rather than hammering; one whose
credential came from a closure refreshes and carries on. Either way the
previously fetched document stays installed and the previous configuration
keeps serving.

## Watching

git has no watch, so `watch` polls — and says so. Each tick is one ref
advertisement, and only a ref that moved costs a transfer. The push half needs
nothing new: whoever terminates a GitHub or GitLab webhook calls
`remote_sink().apply(..)`.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-git)
carries the full story and the credential table; MSRV 1.85.
