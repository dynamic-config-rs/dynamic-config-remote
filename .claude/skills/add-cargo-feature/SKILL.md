---
name: add-cargo-feature
description: Use when adding a Cargo feature to the core dynamic-config crate — covers the five places a feature has to be wired, the redirect-macro pattern for feature-gated generated code, and the checks that catch a half-wired one.
---

# Adding a Cargo feature to `dynamic-config`

A feature is wired in five places, and every one of them has shipped broken
somewhere at least once. `full` missing a feature, a feature table missing a
row, and a `cfg` evaluated in the wrong crate are all silent until a user
hits them.

## The five places

1. **`dynamic-config/Cargo.toml`** — the feature itself, its optional
   dependencies (`dep:` syntax), and *the `full` list*. `full` is documented
   as "all of the above"; a feature it misses makes that sentence a lie.
2. **The two feature tables** — `dynamic-config/src/lib.rs`'s module docs and
   the book's (`book/src/msrv-features.md`). Same rows, same order.
3. **The MSRV story** — if the new dependency raises the floor, that is: a row
   in ci.yml's `msrv` matrix, a line in `just msrv`, a row in the MSRV
   tables (the book's, and the summary in `README.md`), and a note that MSRV
   is *measured* (`cargo check` against the
   real toolchain, not the dependency's declared figure — `age` says 1.74 and
   needs 1.85).
4. **cargo-hack** — nothing to add (`--feature-powerset` picks it up), but run
   `just hack` locally: pairwise interactions are where a wrong `cfg` lives.
5. **The missing-feature diagnostic** — a runtime capability the feature
   gates (a format, `.env` parsing, decryption) is reached through runtime
   data, so the diagnostic is a *load-time* error naming the feature to add
   (`unsupported` in `loader/sections.rs` is the pattern), pinned by an
   ordinary assertion. If the feature gates a *generated method* instead,
   use the redirect-macro pattern below: the method exists with the feature
   and does not without it, proven by a trybuild `.pass()` test that the
   rest still compiles.

## Feature-gated generated code: the redirect-macro pattern

A `#[cfg(feature = "...")]` emitted by the proc macro is evaluated against the
**user's** crate features, where the feature does not exist — the method
silently vanishes for everyone. The pattern that works, used today by
`__async_methods!`, `__async_remote_methods!` and `__clap_methods!`:

```rust
// in expand/ — no cfg, just a call:
::dynamic_config::__the_methods!(#args);

// in dynamic-config/src/redirects.rs — the cfg lives where the feature exists:
#[cfg(feature = "the-feature")]
#[macro_export] #[doc(hidden)]
macro_rules! __the_methods { (...) => { pub fn the_method(...) {...} } }

#[cfg(not(feature = "the-feature"))]
#[macro_export] #[doc(hidden)]
macro_rules! __the_methods { (...) => {} }
```

The three that exist all gate methods whose *signatures* name a feature-gated
type, which is the only job left for redirects: a plain method on `Builder`
takes an ordinary `#[cfg]` in `builder.rs` (`watch`, `schema`), and a runtime
capability fails at load time. The `cfg(not)` arm is empty because the
surrounding type is legitimate without the feature; prove the build both ways
with a trybuild `.pass()` test in a scratch crate that has the feature *off*
— a unit test in this crate cannot catch it, because this crate's own
features leak into the test build.

A new generated method also has to appear in the book's attribute reference
and the lib.rs front-page table — `tests/doc_surface.rs` diffs both against
the macro source, in both directions.

## The checks

```sh
just hack                 # every pair compiles
just test                 # including the ui suites both ways
just msrv                 # if the floor moved
cargo doc -p dynamic-config --all-features   # doc_cfg badges render
```

And recount: the lib.rs table, the book's table and `[features]` must all
have the same number of rows.
