## What this changes

<!-- One or two sentences. What is different afterwards, from a user's side? -->

## Why

<!-- The problem, not the patch. If it fixes an issue, link it: Fixes #123 -->

## The decision, if there was one

<!-- Delete if there wasn't. If you chose between two reasonable designs, say
     which and why — that reasoning is the part a future reader cannot recover
     from the diff. The same goes for anything deliberately *not* done. -->

## Checklist

- [ ] `just check` passes locally (fmt, clippy, tests, docs)
- [ ] New behaviour has a test that would fail without the change
- [ ] Public items have doc comments; a new argument or feature is in `README.md`
- [ ] `CHANGELOG.md` has an entry under `Unreleased`
- [ ] MSRV unchanged, or the change is called out here and in the README table

<!-- For a change to a companion crate, its own README and CHANGELOG too. -->

## Compatibility

- [ ] No public API removed or narrowed
- [ ] No behaviour change a working program would notice

<!-- If either is unchecked, say what breaks and how a user migrates. MSRV
     changes count as breaking. -->
