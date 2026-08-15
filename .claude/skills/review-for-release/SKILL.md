---
name: review-for-release
description: Use before cutting a release, or when asked to review the whole repository — the checks that have actually caught things here, in the order that finds problems fastest.
---

# Reviewing before a release

Run `just check` first. Everything below is what that does not catch.

## The checks that have caught real bugs here

**Run the suite twice: parallel, then `--test-threads=1`.** The macro takes
literal paths, and layers, aliases and bindings live in `static`s — two tests
sharing a config type, a fixture path or an environment variable race in
parallel and pass alone. Both orders have found bugs.

**Grep for `Instant::now() +` and `Duration::from_*` on anything a server
sent.** A lease duration from a compromised or buggy server panics the process
on the arithmetic. `checked_add`, always.

**Check every error path that removes a file.** `save_new` deleted the file it
had just refused to overwrite. Ask: is this call what created the thing being
cleaned up?

**Read every `.expect(` and `panic!` outside tests.** The crate's stance is that
a poisoned lock recovers rather than propagating: none of the data behind those
locks has an invariant a panic could break, and one panicking caller must not
take out every later load.

**Look for values in diagnostics.** Diffs, `check()` reports, unknown-key
suggestions and *error messages* report paths and types, never values.
`tests/security.rs` enforces it; a new message that interpolates a value is a
security regression that no test names.

**Verify MSRV against real toolchains.** `age` declares 1.74 and needs 1.85.
A manifest is a claim, not a measurement. `just msrv` runs every floor.

**Check CI parity.** Any list that appears in more than one place drifts:
the crate lists in the containers job, publish-dry-run, security.yml and
the justfile, the MSRV matrix against the README's table, and the publish
waves in release.yml against the dependency order. Diff them, don't skim
them.

**Audit the stacked `#[cfg]`s.** Two `#[cfg]` attributes on one item AND
together — `#[cfg(unix)] #[cfg(not(unix))]` compiles to nothing, silently.
Three tests here never ran for months because of one. Grep for consecutive
cfg lines and read each pair.

**Check the counts.** Ten crates, ten ci.yml jobs, eleven changelogs,
eight stores. Every one of those numbers appears in documentation
somewhere — the README's table, the book's failure tables, `AGENTS.md` —
and recounting is cheaper than a reader finding the stale one.

**`cargo clippy -- -W clippy::pedantic`** for the substantive lints only:
`unnecessary_wraps`, `needless_pass_by_value` on public API, `redundant_clone`,
`must_use_candidate`. Ignore the style noise.

## Documentation

- Every anchor and relative link resolves. A dead `#anchor` in a README is
  small, easy to ship, and the first thing a reader hits.
- Counts anywhere in the documentation — tests, examples, features, crates —
  match reality. Run the suite and count rather than trusting the last number.
- The book's example output matches what the examples print. Run them.
- Each store's chapter has a failure table, and every branch its watch loop
  can take is a row. A branch that reports `reachable()` differently from
  its neighbours is the bug this repository has already shipped once.
- Each crate's README is *its own*, not the repository's.

## Release mechanics

- The workspace version, the tag and every crate's changelog agree.
- `cargo package --list` for each crate: README and LICENSE present, tests
  and benches excluded.
- The engine requirement is still a caret. An exact pin here would make an
  engine upgrade wait for this repository.
- Never run `cargo publish`. `cargo release` prepares; merging into `main`
  publishes, in two waves.
