# Security

## Reporting a vulnerability

Please report privately, through
[GitHub's advisory form](https://github.com/ctolon/dynamic-config/security/advisories/new),
rather than in a public issue.

Include what you would want if you were on the other side: what an attacker can
do, what they need to start with, and — if you have one — a reproduction. A
first response should take a few days; if it takes longer than a week, assume
the message went astray and ping the issue tracker without details.

## What this crate is responsible for

A configuration library sits in an awkward place: it reads files a program
trusts, holds values a program must not leak, and — with the companion crates —
talks to services over a network. These are the properties it tries to keep, and
the ones it explicitly does not.

### It tries to keep

**Secrets stay out of diagnostics.** `#[config(secret)]` redacts a field in
`Debug`; reload diffs, `check()` reports, unknown-key suggestions and *error
messages* all report paths and types, never values, so nothing routes around the
redaction. A report of a value appearing in a log or an error message is a
vulnerability. `dynamic-config/tests/security.rs` asserts each of these, and CI
runs it as a job of its own.

**Files it writes are private from the moment they exist.** `save` and the
last-known-good cache create their file `0600` on Unix — created that way, not
chmodded afterwards — through a temporary path opened with `create_new`, so a
symlink planted at that path is refused rather than followed.

**A remote store cannot crash the process.** Everything a server sends is
treated as untrusted input: lease durations that would overflow a clock, bodies
that are not JSON, values that are not UTF-8. A panic reachable from a server's
response is a vulnerability.

**Credentials are not logged.** A `Debug` on a source prints its address and its
key, never a token, a password or a key file's contents.

### It explicitly does not

**Keep secrets out of process memory.** A resolved configuration holds every
value, because that is what configuration is — a program that can use a password
can read it. The `age` feature zeroizes the decrypted *file* once parsed, which
is defence in depth, not a claim about memory.

**Encrypt what it writes.** `save` and the cache write plaintext. The cache's
three modes exist so that is a choice rather than a surprise; see
[the book](book/src/persistence.md#last-known-good).

**Validate that a config file is trusted.** If an attacker can write your config
file, they can configure your program. That is the file's permissions to
enforce, not this crate's.

**Sandbox a `Decryptor` or a `RemoteSource` you implement.** Those run your code
with your privileges.

## Supported versions

| Version | Supported |
|---|---|
| 0.0.x | ✅ the latest patch |
| < 0.0.1 | — nothing older exists |

Before 1.0, fixes land on the latest published version and nothing is
backported: there is no version old enough to be worth pinning to. After 1.0,
the current and previous minor versions.

## Threat model, stated plainly

This crate assumes:

- **The config files are trusted.** Whoever can write them can configure the
  program, which is the point of a config file. File permissions are the
  control, not this crate.
- **The environment is trusted.** Same reasoning: anything that can set
  `APP_DB_HOST` is already inside the process's world.
- **A remote store is *not* trusted.** It is across a network, may be
  compromised, and may simply be buggy. Everything it sends is parsed
  defensively; nothing it sends can panic the process or reach an arithmetic
  overflow.
- **A `Decryptor` or `RemoteSource` you write is trusted.** It runs your code.
- **Log output is not private.** Everything this crate writes to a log or an
  error may end up in a system that many people can read, which is why
  diagnostics carry paths and types rather than values.

## How the properties are kept honest

Prose is not a guarantee, so each claim above has something behind it that CI
runs on every change:

| Claim | Enforced by |
|---|---|
| Secrets stay out of diagnostics | `dynamic-config/tests/security.rs`, run as its own CI job |
| Files are created private, symlinks refused | `write::permissions` tests, same job |
| A store cannot panic the process | `checked_add` on every server-supplied duration, plus a hostile-document test |
| No unsafe code | `#![forbid(unsafe_code)]` in every crate, *and* a CI job that checks the attribute is still there |
| No unmaintained or vulnerable dependency | `cargo deny`, on every push and weekly on a schedule |
| No surprise dependency | `cargo deny` sources and licences, plus dependency review on pull requests |

Exceptions in `deny.toml` are listed one at a time with a reason. There is no
blanket "ignore dev-dependencies": something that runs on a contributor's
machine or in CI still matters.

## Dependencies

The core crate's non-optional dependency list is deliberately short — `figment`,
`serde`, `arc-swap` — and every network client, format parser and crypto stack
is behind a feature or in a companion crate, so a build carries only what it
asked for. A `dynamic-config` with default features pulls in no cryptography, no
HTTP client and no runtime.

`cargo deny` runs on every push and weekly on a schedule, because an advisory is
published on somebody else's timetable: a dependency that was clean on Monday is
not necessarily clean on Friday, and nothing about this repository has to change
for that to happen.

## What a report gets you

A fix, credit unless you would rather not have it, and a note in the changelog
under `Security` describing what was wrong in enough detail to tell whether you
were affected. If a fix has to be coordinated with a downstream project, that is
worth saying in the first message.
