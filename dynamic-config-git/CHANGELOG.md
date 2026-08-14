# Changelog

All notable changes to `dynamic-config-git` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is breaking.

<!-- Keep this template. Add entries under `Unreleased` as you go, and move
     the whole block under a new version heading at release time.
     (Spelled `_Unreleased_` here so cargo-release's `exactly = 1` search
     for the real heading matches only the real heading.)

## [_Unreleased_]

### Added
### Changed
### Deprecated
### Removed
### Fixed
### Security

-->

## [Unreleased]

## [0.6.1] — 2026-08-14

## [0.6.0] — 2026-08-13

### Added

- **First release.** Reads a file — or a set of them — at one ref, from one git repository, over
  whatever transport git speaks — so GitHub, GitLab, Azure DevOps, Gitea,
  Bitbucket and a self-hosted server behind a VPN are all the same client
  rather than five. Implemented on `gix`: pure Rust, no `libgit2`, no C
  toolchain and no OpenSSL question. The blocking `RemoteSource` trait,
  because a git fetch is blocking work.
- **A fetch is shallow, single-ref and blobless of history.** The ref
  advertisement is read first — what `git ls-remote` costs — and objects are
  asked for only when the commit is one the working directory does not have.
  An unchanged ref transfers nothing, which is what makes polling a git host
  at configuration cadence defensible.
- **Branch, tag or commit id**, with a branch as the default: a store whose
  reason to exist is that the configuration changes should not default to a
  ref that cannot.
- **Every credential can come from a callable.** `Credential::from_fn` is
  re-read per fetch; `Credential::expiring` is refreshed within a minute of the
  expiry the issuer reported and thrown away the moment the host refuses it,
  through `dynamic-config-store-core`'s cache. A GitHub App installation token
  lives an hour and a watcher lives for the life of the process, which is the
  whole reason the credential is a function rather than a string.
- **HTTPS with a token, SSH through an agent or a named key, and anonymous
  access** to a public repository. SSH is carried by the system `ssh`, the way
  `git` carries it, so `ssh` must be on the host.
- A working directory that is `0700` from the moment it exists, temporary by
  default and removed with the source, or named by the caller to survive
  restarts.
- **`Builder::compact_after(n)`, and a working directory that stops growing.**
  A shallow fetch of a moving branch adds a pack every time the branch moves
  and removes nothing, so a watcher left running for a month grew without
  bound. Every thirty-second transfer by default, the object database is
  emptied and refilled by the fetch that emptied it — one full transfer per
  thirty-two pushes, a bounded fraction of what was going to transfer anyway.
  `0` turns it off for a deployment that would rather run `git gc
  --prune=now` on its own cadence.

  The rule it deletes under is narrow enough to state in one line: **only what
  it wrote, only in a directory it created for itself, and only on a trigger
  the caller can see and turn off.** A `cache_dir` pointing at a repository
  that already existed carries no marker file and is never touched, whatever
  `compact_after` says.

- **`max_bytes` bounds the whole read rather than one file of it.** It used to
  bound a single file, so a directory read was bounded at the file limit times
  the key limit — half a gigabyte at the defaults, a product nobody chose and
  nobody would have. A caller who says a megabyte is saying what this source
  may load, and there is no reading of that under which naming a directory
  multiplies it by five hundred. The count limit stays, because the two bound
  different things: a tree of a hundred thousand empty files costs the walk
  rather than the memory.

- **`with_timeout` reaches the connection and a host that goes quiet**, on the
  transport `tls` installs. `gix` bounds a fetch with an interrupt flag it
  checks between packets, which is a real deadline for the part that transfers
  data and none at all for a host that accepts the connection and then sends
  nothing — there are no packets for the check to be between, so a ref
  advertisement could park a thread for as long as the host cared to hold the
  socket open. On this crate's client the caller's number is now the connect
  deadline *and* the stall deadline on every read, and a fetch that hits it says
  which number it hit rather than rendering as an outage.

  Not on `gix`'s own transport, which is what a source without `tls` uses:
  measured against `gix-transport` 0.58.1, its `reqwest` backend hardcodes
  twenty seconds for connecting and reads neither the `connect_timeout` its own
  options type carries nor any other. Closing that from outside would have meant
  routing every `https://` source through this crate's client, costing redirect
  following and `http.extraHeader` for callers who never asked for either, so
  what is documented instead is a table of which transport bounds which phase.

- **Several files as one document**, through `Keys`. `.path(..)` takes a path
  as it always did, or `Keys::several([..])` — merged in call order, later wins
  — or `Keys::prefix("conf")`, every file under a directory merged as disjoint
  sections with an overlap refused by name. The fold, the ordering rules, the
  512-file budget and the collision report are
  `dynamic-config-store-core::documents`, shared with the seven other stores. A
  directory rather than a string prefix, because a git tree has directories:
  `Keys::prefix("conf")` does not read `conf-old.yaml`.
- **A multi-file source can be watched**, which is true of no other store here.
  A fetch resolves one commit and a commit has one tree, so a set of files is
  read as of one instant with no transaction and no listing race — the
  interleaving that made every other store refuse a watch on a set cannot
  happen. It needed no new code path: the watch loop already re-reads through
  the same call.
- **TLS with a custom certificate authority and client certificates**, through
  `Builder::tls` and `dynamic-config-store-core`'s `TlsConfig` — the same
  vocabulary the other store crates take. A CA bundle from a file or from
  bytes, a client certificate and key for mTLS, and the platform's own trust
  store still applying underneath. It is the `https://` knob only: a `tls` on
  an `ssh://` url is refused at `build()`, because that transport's trust is
  `known_hosts` and its identity is a key.

### Changed

- **An error carries its whole source chain.** `gix` renders only the top of
  it, so a fetch that failed inside the transport used to read "An IO error
  occurred when talking to the server" and nothing else; the reason — an
  unknown certificate issuer, an HTTP status — is two `source()` calls below
  and is now part of the message.

### Fixed

- **A credential in the URL no longer reaches `Builder`'s `Debug`.**
  `GitSource` redacts it and the builder derived its own, so
  `https://user:token@host/repo.git` printed verbatim for the whole life of
  the builder — which is during construction, the place people print things
  to see what they have configured. Hand-written now, like the source's, and
  the planted-token test covers both.

### Security

- **A remote URL is redacted everywhere it is printed.** `Debug`, `describe()`
  and every error message put it through `dynamic-config-store-core`'s
  redaction first, which splits on the last `@` because a token may contain
  one. An authority with no colon in it is read as a **secret**, not a user
  name, because `https://ghp_…@github.com/…` is a documented GitHub form in
  which the whole authority is the token.
- **Nothing from the repository is ever written to the filesystem.** The
  working directory is a bare object database and no checkout ever happens, so
  a tree containing a symlink to `/etc/shadow` — or an entry named
  `../../etc/shadow` — cannot make this crate write or read outside it. A
  symlink is refused rather than followed, as are a directory, a submodule, a
  blob over `max_bytes` and a blob that is not UTF-8.
- **A credential is never sent in clear text.** `gix`'s refusal to put one on
  an `http://` connection is left on; the test suite turns it off through a
  dev-dependency for a loopback socket and nowhere else.
- **A refused credential is `ErrorKind::Auth`**, so a reload loop stops rather
  than retrying a token that will never be accepted. One replacement and one
  retry, not a loop.
- **A tree entry's own name is untrusted too.** A `Keys::prefix` walk builds
  paths out of names the remote chose, so every path it discovers goes back
  through the same check a configured one gets: no `.`, no `..`, no empty
  component, and it must still be under the directory asked for. No porcelain
  writes an entry called `..`, but `git mktree` does and so can a pack from a
  host nobody controls — which is how the test for it is written.
- **There is no `skip_verification`, deliberately.** Trusting one more
  certificate is what a private CA and a self-signed development server both
  need, and it keeps the server authenticated; turning verification off does
  not make TLS weaker in the way a checklist means, it makes it absent. git
  sharpens that: a fetch presents its credential before it has received
  anything, so an unverified connection is one that hands a token to whoever
  is on the path.
- **A private key never reaches a diagnostic.** The client certificate and its
  key go from `TlsConfig` into the TLS client and stop; a PEM that will not
  parse is an error naming the setting and never the bytes, and a missing file
  is an error naming the path. Tested with planted material on every new error
  path, including the one `reqwest` renders rather than this crate.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0

- **A working directory can no longer be claimed twice under two
  spellings.** The same-process claim compared paths lexically, so `cache`
  and `./cache` — or two symlinks to one directory — were two claims, each
  with its own fetch mutex: two sources fetching into one object database,
  where one's `compact` can empty it while the other is reading. The claim
  is on the resolved directory now, and holds for a directory the fetch has
  not created yet.

