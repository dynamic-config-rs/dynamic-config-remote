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
   the root README's "Cargo features" section. Same rows, same order.
3. **The MSRV story** — if the new dependency raises the floor, that is: a row
   in ci.yml's `msrv` matrix, a line in `just msrv`, a row in the README's
   MSRV table, and a note that MSRV is *measured* (`cargo check` against the
   real toolchain, not the dependency's declared figure — `age` says 1.74 and
   needs 1.85).
4. **cargo-hack** — nothing to add (`--feature-powerset` picks it up), but run
   `just hack` locally: pairwise interactions are where a wrong `cfg` lives.
5. **Compile-fail diagnostics** — if using the feature's API without the
   feature should *say so*, add the `compile_error!` redirect and a trybuild
   case in `tests/ui-no-*`; if the generated method should simply not exist,
   the empty redirect arm (below) and a `.pass()` test that the rest still
   compiles.

## Feature-gated generated code: the redirect-macro pattern

A `#[cfg(feature = "...")]` emitted by the proc macro is evaluated against the
**user's** crate features, where the feature does not exist — the method
silently vanishes for everyone. The pattern that works:

```rust
// in expand.rs — no cfg, just a call:
::dynamic_config::__the_method!(#args);

// in dynamic-config/src/lib.rs — the cfg lives where the feature exists:
#[cfg(feature = "the-feature")]
#[macro_export] #[doc(hidden)]
macro_rules! __the_method { (...) => { pub fn the_method(...) {...} } }

#[cfg(not(feature = "the-feature"))]
#[macro_export] #[doc(hidden)]
macro_rules! __the_method { (...) => {} }        // or compile_error!
```

The `cfg(not)` arm is **empty** when the surrounding functionality is
legitimate without the feature (`save` without `decrypt`), and a
`compile_error!` naming the feature when it is not. Prove it with a trybuild
`.pass()` test in a scratch crate that has the feature *off* — a unit test in
this crate cannot catch it, because this crate's own features leak into the
test build.

## The checks

```sh
just hack                 # every pair compiles
just test                 # including the ui suites both ways
just msrv                 # if the floor moved
cargo doc -p dynamic-config --all-features   # doc_cfg badges render
```

And recount: the lib.rs table, the README table and `[features]` must all
have the same number of rows.
