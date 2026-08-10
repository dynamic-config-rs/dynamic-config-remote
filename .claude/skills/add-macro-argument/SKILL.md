---
name: add-macro-argument
description: Use when adding an argument to the #[dynamic_config] attribute — covers parsing, the generated code, feature gating, compile-fail diagnostics, and the documentation an argument has to carry.
---

# Adding a `#[dynamic_config]` argument

The attribute has twenty arguments. Adding one touches four places, and
missing any of them ships something half-wired.

## 1. Parse it — `dynamic-config-macros/src/args.rs`

- A field on `Raw` (`Option<Span>` for a flag, `Option<T>` for a value) and on
  `Args`.
- An arm in the `match`. `parse_string_array` consumes its own `=`; the
  scalar parsers do not. Getting that wrong produces `expected \`=\``.
- The argument's name in the "unknown argument" message. That list is a
  compile-fail expectation, so it has to stay sorted the way it reads.
- A validation rule *only* if the argument would silently do nothing without
  something else — `debounce` without `watch` is that; `diff` is not, because a
  reload has two triggers.

## 2. Generate it — `dynamic-config-macros/src/expand/`

`mod.rs` orchestrates; the methods themselves are built by the submodule that
owns their theme (`accessors`, `watch`, `persistence`, `remote`, `schema`,
`diagnostics`, `async_api`) and spliced back in a fixed order. Put the new
code in the submodule it belongs to, not in `mod.rs`.

Keep the generated code thin. Everything with behaviour belongs in
`dynamic-config` as an ordinary function that can be linted, stepped through and
unit tested; generated code can be none of those.

If the argument needs a `static`, use `slot(..)` — it handles the generic case
(a registry keyed by `TypeId`) and the non-generic case (a plain `static`)
together.

## 3. Gate it, if it needs a feature

Two mechanisms, and the choice is forced:

- **The signature names a feature-gated type** → an item-level redirect macro
  (`__async_methods!`, `__clap_methods!`, `__schema_methods!`). A signature
  cannot hide behind an expression-level `compile_error!`.
- **Everything else** → an expression-level redirect (`__format_json!`,
  `__source_encrypted!`, `__require_dotenv!`).

The message names the feature to add. A silent runtime failure on the one
machine that uses the argument is worse than a build that will not start.

## 4. Document and pin it

- The book: a row in `book/src/attribute-reference.md`'s at-a-glance table
  **and** a section of its own with a runnable example, in the chapter the
  argument belongs to. The thin table on the `dynamic_config` macro item in
  `dynamic-config/src/lib.rs` gets the same row. A new *generated method*
  that skips the book fails `tests/doc_surface.rs`.
- A test that would fail without the argument.
- A compile-fail case in `tests/ui/` if it has a diagnostic; `just bless`
  regenerates the expectation. A diagnostic that only exists when a feature is
  *off* goes in `tests/ui-no-decrypt/` with its own `#[cfg(not(feature = ..))]`
  test — a build with the feature on cannot see it.
- A `CHANGELOG.md` entry.

## The trap

`where Self: SomeTrait` on an inherent method **does not work**: rustc rejects
an inherent method whose bound a concrete `Self` does not meet, at the
*definition* rather than at the call. That is why `save` and `schema` are
arguments rather than methods everyone gets. If a new argument needs a trait the
user derives, it has to be opt-in the same way.
