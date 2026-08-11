---
name: add-macro-argument
description: Use when adding a configuration option to dynamic-config — options go on the runtime Builder and LoadSpec, not on the attribute, which takes no arguments. Covers the Builder method, the LoadSpec plumbing, feature gating, diagnostics, and the documentation an option has to carry.
---

# Adding a `Builder` option

`#[dynamic_config]` takes **no arguments** — the attribute declares, the
builder configures. A new way to state where configuration comes from is a
new method on `Builder` plus a knob on `LoadSpec`. There is no parse step
any more; do not add one. `dynamic-config-macros/src/args.rs` exists only to
reject arguments with a migration map — the only reason to touch it is to
extend that map.

Adding an option touches four places, and missing any of them ships
something half-wired.

## 1. The knob — `dynamic-config/src/source.rs`

`LoadSpec` gets a field and a `with_*` method. `LoadSpec` is built with
`with_*` methods rather than a struct literal precisely so a new knob does
not break every call site at once.

If the option is a new *layer*, `loader::build` in `loader/mod.rs` gets a
merge in the right place, with a comment arguing for the position, and
`origin_of` gets a way to recognise it — the precedence order lives in
`loader/mod.rs` and nowhere else.

## 2. The method — `dynamic-config/src/builder.rs`

A field on `Builder<T>` (mirror it in the manual `Clone`), a chainable
method that takes and returns `self`, and a line in the private `with_spec`
funnel that turns the field into the `LoadSpec` call. `with_spec` is the one
place the builder meets the spec; an option wired anywhere else can drift.

Builder methods are infallible by design — a missing file or an unsupported
value is a load-time answer, not a construction-time one. If the option only
makes sense with knowledge the generated `builder()` carries (secrets, field
names, the installer), refuse it at `init` with an error that says to start
from the generated `builder()` — the redacted cache modes are the pattern.

## 3. Gate it, if it needs a feature

- **A runtime capability** (a format, `.env` parsing, decryption): compile
  the implementation out behind the feature and make using it a *load-time*
  error naming the feature to add — see `unsupported` in
  `loader/sections.rs`. The paths are runtime data, so compile time cannot
  see the problem.
- **A generated method whose signature names a feature-gated type**: an
  item-level redirect macro in `dynamic-config/src/redirects.rs`
  (`__async_methods!`, `__clap_methods!`, `__async_remote_methods!` are the
  three that exist). The `cfg` must live in the facade crate — one emitted
  by the proc macro is evaluated against the user's features. See the
  add-cargo-feature skill.
- **A Builder method behind a feature** needs no redirect at all: `builder.rs`
  is ordinary code, so a plain `#[cfg(feature = ..)]` on the `impl` block
  works — `watch` and `schema` are the pattern.

## 4. Document and pin it

- The book: a row in the Builder tables in
  `book/src/attribute-reference.md`, **and** a section of its own with a
  runnable example in the chapter the option belongs to. The attribute has
  no argument table any more; do not resurrect one.
- If the option changes what the *attribute* generates, the generated method
  must appear in the book's "What the attribute generates" table and the
  lib.rs front-page table — `tests/doc_surface.rs` diffs both against the
  macro source, in both directions.
- A test that would fail without the option — builder tests live in
  `dynamic-config/tests/builder.rs`.
- A diagnostic's exact wording goes in `tests/ui/` with `just bless` if it
  is a compile error, or an ordinary assertion if it is a load-time error —
  and it must report paths and types, never values.
- A `CHANGELOG.md` entry.

## The trap

`where Self: SomeTrait` on an inherent method **does not work**: rustc
rejects an inherent method whose bound a concrete `Self` does not meet, at
the *definition* rather than at the call. That is why `schema()` lives on
`Builder`'s own generic `impl` (which can state `T: JsonSchema`) and `save`
is a free function over any `Serialize` value. An option that needs a trait
the user derives belongs on the builder or as a free function, never as a
generated inherent method.
