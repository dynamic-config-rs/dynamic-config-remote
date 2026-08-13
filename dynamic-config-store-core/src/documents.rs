//! Folding several keys into the one document `fetch` returns.
//!
//! A store that reads a prefix — or a list of keys the caller named — still
//! has to hand the loader a single [`Fetched`], because [`Fetched`] carries one
//! text and one format on purpose and widening it would change a trait every
//! external store implements. So the fold happens in the store, before `fetch`
//! returns, and this is the part of it that is the same in every store: the
//! ordering rule, the collision report, and the two limits an untrusted key
//! list has to be held to.
//!
//! What is *not* here is how a store finds its keys. etcd has a range read,
//! Consul has `?recurse`, Redis has `SCAN` — one call each, and nothing about
//! them generalises.
//!
//! # The two rules, and why they differ
//!
//! A caller who **names keys** is expressing an order, exactly the way a
//! caller who names files is: `.file("base.toml").file("local.toml")` merges
//! in call order and the later file wins. So does [`Overlap::LaterWins`].
//!
//! A caller who names a **prefix** is expressing something else — "these are
//! the sections of my configuration" — and the order the server lists them in
//! is not a precedence anybody chose. Two keys under one prefix supplying the
//! same path is a deployment bug, so [`Overlap::Refused`] reports it instead of
//! resolving it. The report names the two keys and the paths, never the values.

use dynamic_config::{Error, Fetched, Format, Value};

/// The most keys one prefix read will fold into a document.
///
/// A server can answer a range read with anything at all, and the answer is
/// held in memory twice over — once as text, once parsed — before it becomes a
/// document. Five hundred sections is already far past what a configuration
/// has and far short of what a mistyped prefix (`""`, or a prefix that is a
/// whole tenant's key space) would return, which is the pair of numbers this
/// has to sit between.
///
/// It bounds a *prefix* read only. An explicit list of keys was written down
/// by the caller, so its length is theirs to choose.
pub const MOST_KEYS: usize = 512;

/// How many colliding paths a refusal names before it stops counting.
///
/// Two documents that overlap completely would otherwise put every leaf they
/// have into one error message.
const MOST_REPORTED_PATHS: usize = 8;

/// What happens when two of a source's keys supply the same path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlap {
    /// Later wins, in the order the documents are given.
    ///
    /// For a list of keys the caller wrote down: the list *is* the precedence,
    /// the same way a list of `.file(..)` calls is. Tables merge deeply and
    /// arrays are replaced whole — the rule
    /// [`Value::merge`](dynamic_config::Value::merge) already implements,
    /// because it is the rule every layer in this family already means.
    LaterWins,
    /// An overlap is an error naming both keys and the paths.
    ///
    /// For a prefix read, where the keys arrive in whatever order the server
    /// felt like and no order between them would be defensible.
    Refused,
}

/// Folds `documents` into the one document a `fetch` returns.
///
/// `documents` is `(key, text)` in the order the merge should apply. `key` is
/// only ever used to name a document in a diagnostic; `described` is the
/// store's own [`describe()`](dynamic_config::RemoteSource::describe).
///
/// **A single document is passed through byte for byte.** It is not parsed and
/// not re-rendered, so a one-key read costs nothing, cannot fail on a format
/// whose feature is off, and produces exactly the bytes that were stored. Two
/// or more are parsed, merged, and rendered back into `format` — which does
/// need that format's feature, and says so if it is missing.
///
/// **A partial read is not a partial document.** Whatever calls this has
/// already decided that every key was readable: one unreadable key out of five
/// must fail the whole fetch rather than merge the four, because a
/// configuration silently missing a section is worse than a refresh that
/// failed and left the last known good document serving.
///
/// # Errors
///
/// If `documents` is empty, if any document does not parse, if the merged tree
/// cannot be rendered back into `format`, or — under [`Overlap::Refused`] — if
/// two documents supply the same path.
pub fn merged(
    documents: &[(String, String)],
    format: Format,
    overlap: Overlap,
    described: &str,
) -> Result<Fetched, Error> {
    match documents {
        [] => Err(Error::remote(format!(
            "{described}: no key held a value, so there is nothing to load"
        ))),

        // Byte for byte: parsing and re-rendering one document would rewrite
        // key order and drop comments for no gain at all, and would make a
        // single-key read need a format feature it never needed before.
        [(_, only)] => Ok(Fetched::new(only.clone(), format)),

        _ => Ok(Fetched::new(
            folded(documents, format, overlap, described)?
                .render(format)
                .map_err(|error| {
                    Error::remote(format!(
                    "{described}: the merged document cannot be written back as {format:?}: {error}"
                ))
                })?,
            format,
        )),
    }
}

/// The merge itself, kept apart so [`merged`] reads as the three cases it is.
fn folded(
    documents: &[(String, String)],
    format: Format,
    overlap: Overlap,
    described: &str,
) -> Result<Value, Error> {
    // Kept rather than folded away, so a refusal can say which *earlier* key
    // supplied the path the current one collided with. Only the error path
    // walks them, so the cost is a vector of trees that were parsed anyway.
    let mut parsed: Vec<(&str, Value)> = Vec::with_capacity(documents.len());

    for (key, text) in documents {
        let value = Value::parse(text, format).map_err(|error| {
            Error::remote(format!(
                "{described}: `{key}` is not a {format:?} document: {error}"
            ))
        })?;

        parsed.push((key.as_str(), value));
    }

    let mut folded = parsed[0].1.clone();

    for (key, value) in &parsed[1..] {
        if overlap == Overlap::Refused {
            let clashes = folded.overlapping_paths(value);

            if !clashes.is_empty() {
                return Err(collision(&parsed, key, &clashes, described));
            }
        }

        folded.merge(value.clone());
    }

    Ok(folded)
}

/// The report two overlapping keys earn: both key names, and the paths.
///
/// Paths and never values — [`Value::overlapping_paths`] is built that way, and
/// a collision report is a diagnostic, which in this family means it names what
/// moved and never what was there.
fn collision(parsed: &[(&str, Value)], key: &str, clashes: &[String], described: &str) -> Error {
    // Which earlier key actually supplied the first colliding path. The merged
    // tree cannot say — it is every earlier key at once — so the answer comes
    // from the documents themselves, and only here, on the path that is already
    // failing.
    let before = parsed
        .iter()
        .position(|(name, _)| *name == key)
        .unwrap_or(parsed.len());

    let earlier = clashes
        .first()
        .and_then(|path| {
            // Backwards: with later-wins off nobody won, but the *nearest*
            // earlier key is the one a reader will look at first.
            parsed[..before]
                .iter()
                .rev()
                .find(|(_, value)| value.get(path).is_some())
        })
        .map_or("an earlier key", |(name, _)| *name);

    let named: Vec<&str> = clashes
        .iter()
        .take(MOST_REPORTED_PATHS)
        .map(String::as_str)
        .collect();

    let more = clashes.len().saturating_sub(named.len());
    let and_more = if more == 0 {
        String::new()
    } else {
        format!(" (and {more} more)")
    };

    Error::remote(format!(
        "{described}: `{earlier}` and `{key}` both supply {}{and_more}; keys read \
         as a prefix are sections that must not overlap, and the order a server \
         lists them in is not a precedence — name the keys instead if one is \
         meant to win",
        named.join(", ")
    ))
}

/// The format every key's extension agrees on.
///
/// One source reads one format: [`Fetched`] carries one, the merge happens in
/// one, and a caller who wants a JSON key and a TOML key has two sources rather
/// than one — which already works and is what the tedium this feature removes
/// was never about.
///
/// So a list whose extensions disagree is a mistake worth catching by name.
/// Parsing `myapp/server.toml` as JSON because `myapp/db.json` came first
/// produces a syntax error about a document that has no syntax error in it,
/// which is a bad half-hour for whoever gets it.
///
/// `Ok(None)` when no key names a format at all — the store's `with_format` is
/// the answer, exactly as it is for one key with no extension.
///
/// # Errors
///
/// The two keys that disagree, worded for a store to quote into its own error.
/// Never a value: a key name is caller input, and it is all this sees.
pub fn agreed_format(keys: &[String]) -> Result<Option<Format>, String> {
    let mut agreed: Option<(&str, Format)> = None;

    for key in keys {
        let Some(format) = Format::from_key(key) else {
            continue;
        };

        match agreed {
            Some((named, first)) if first != format => {
                return Err(format!(
                    "`{named}` names {first:?} and `{key}` names {format:?}; one source \
                     reads one format — call `with_format` to settle it, or install one \
                     source per format"
                ));
            }
            Some(_) => {}
            None => agreed = Some((key, format)),
        }
    }

    Ok(agreed.map(|(_, format)| format))
}

/// Refuses a key list longer than a configuration has any business being.
///
/// A prefix is caller input and the answer to it is server input: an empty
/// prefix, or one pointed at a whole tenant's key space, matches everything
/// there is. Called *before* the values are read where the protocol allows the
/// count to be known first, and immediately after the one call that carries
/// them where it does not.
///
/// # Errors
///
/// If `matched` is above [`MOST_KEYS`].
pub fn within_key_budget(matched: usize, described: &str) -> Result<(), Error> {
    if matched <= MOST_KEYS {
        return Ok(());
    }

    Err(Error::remote(format!(
        "{described}: the prefix matches {matched} keys, above the {MOST_KEYS} \
         one document is folded from; narrow the prefix"
    )))
}

/// Refuses a key the server returned that is not under the prefix that was
/// asked for.
///
/// Not paranoia about a lying server so much as about the *query*: Redis'
/// `SCAN MATCH` takes a glob, so a prefix containing `*`, `?` or `[` would
/// match keys the caller never named, and Consul's `?recurse` is a string
/// prefix that a proxy could rewrite. The literal check is one comparison and
/// it makes the prefix mean what it says in every store.
///
/// # Errors
///
/// If `key` does not start with `prefix`.
pub fn under_prefix(key: &str, prefix: &str, described: &str) -> Result<(), Error> {
    if key.starts_with(prefix) {
        return Ok(());
    }

    Err(Error::remote(format!(
        "{described}: the store answered with `{key}`, which is not under the \
         prefix that was asked for"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, text)| ((*key).to_owned(), (*text).to_owned()))
            .collect()
    }

    /// The rule a list of keys inherits from a list of files: call order, and
    /// the later one wins.
    #[test]
    fn a_named_list_merges_in_order_and_the_later_key_wins() {
        let documents = documents(&[
            ("myapp/base", r#"{"db": {"host": "a", "port": 1}}"#),
            ("myapp/local", r#"{"db": {"port": 2}}"#),
        ]);

        let fetched = merged(&documents, Format::Json, Overlap::LaterWins, "store")
            .expect("two documents merge");

        let tree = Value::parse(&fetched.text, Format::Json).expect("the result is a document");

        assert_eq!(tree.get("db.host"), Some(&Value::String("a".to_owned())));
        assert_eq!(tree.get("db.port"), Some(&Value::Integer(2)));
    }

    /// Disjoint sections are the case a prefix read is *for*, and it has to
    /// work identically under either rule.
    #[test]
    fn disjoint_sections_fold_into_one_document_under_either_rule() {
        let documents = documents(&[
            ("myapp/db", r#"{"db": {"host": "a"}}"#),
            ("myapp/server", r#"{"server": {"port": 8080}}"#),
        ]);

        for overlap in [Overlap::LaterWins, Overlap::Refused] {
            let fetched =
                merged(&documents, Format::Json, overlap, "store").expect("nothing overlaps");
            let tree = Value::parse(&fetched.text, Format::Json).expect("the result is a document");

            assert_eq!(tree.get("db.host"), Some(&Value::String("a".to_owned())));
            assert_eq!(tree.get("server.port"), Some(&Value::Integer(8080)));
        }
    }

    /// The refusal names both keys and the path, so the person reading it can
    /// go and fix the deployment.
    #[test]
    fn a_prefix_collision_names_both_keys_and_the_path() {
        let documents = documents(&[
            ("myapp/db", r#"{"db": {"host": "a"}}"#),
            ("myapp/server", r#"{"server": {"port": 1}}"#),
            ("myapp/extra", r#"{"db": {"host": "b"}}"#),
        ]);

        let error = merged(&documents, Format::Json, Overlap::Refused, "store")
            .expect_err("two keys supply db.host");

        let printed = error.to_string();

        assert!(printed.contains("myapp/db"), "{printed}");
        assert!(printed.contains("myapp/extra"), "{printed}");
        assert!(printed.contains("db.host"), "{printed}");
        // The key that did not collide is not dragged into the report.
        assert!(!printed.contains("myapp/server"), "{printed}");
    }

    /// The rule the whole repository is built around, at the one new error
    /// path that has both documents in its hands.
    #[test]
    fn a_collision_report_names_paths_and_never_values() {
        let documents = documents(&[
            ("myapp/db", r#"{"db": {"password": "hunter2-left"}}"#),
            ("myapp/extra", r#"{"db": {"password": "hunter2-right"}}"#),
        ]);

        let error = merged(&documents, Format::Json, Overlap::Refused, "store")
            .expect_err("both keys supply db.password");

        let printed = format!("{error} {error:?}");

        assert!(printed.contains("db.password"), "{printed}");
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    /// A document that does not parse names the key it came from — with a
    /// prefix read the whole point is that the caller does not know which key
    /// is which until something says so.
    #[test]
    fn a_document_that_does_not_parse_names_its_key_and_not_its_contents() {
        let documents = documents(&[
            ("myapp/db", r#"{"db": {"host": "a"}}"#),
            ("myapp/broken", r#"{"password": "hunter2"#),
        ]);

        let error = merged(&documents, Format::Json, Overlap::LaterWins, "store")
            .expect_err("the second document is truncated");

        let printed = format!("{error} {error:?}");

        assert!(printed.contains("myapp/broken"), "{printed}");
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    /// One key is the old behaviour, and has to stay byte-identical: a
    /// round trip through the tree would reorder keys and drop comments.
    #[test]
    fn one_document_is_handed_over_exactly_as_it_was_stored() {
        let stored = "{\n  \"zebra\": 1,\n  \"apple\": 2\n}\n";

        let fetched = merged(
            &documents(&[("myapp/db", stored)]),
            Format::Json,
            Overlap::Refused,
            "store",
        )
        .expect("one document needs no merge");

        assert_eq!(fetched.text, stored);
    }

    #[test]
    fn no_keys_at_all_is_a_failure_rather_than_an_empty_document() {
        let error = merged(&[], Format::Json, Overlap::Refused, "store")
            .expect_err("an empty set is not a configuration");

        assert!(error.to_string().contains("nothing to load"), "{error}");
    }

    /// The budget is what stands between a mistyped prefix and a process that
    /// pulls a whole key space into memory.
    #[test]
    fn a_prefix_matching_more_keys_than_the_budget_is_refused() {
        within_key_budget(MOST_KEYS, "store").expect("the budget itself is allowed");

        let error = within_key_budget(MOST_KEYS + 1, "store").expect_err("one too many");

        assert!(error.to_string().contains("narrow the prefix"), "{error}");
    }

    #[test]
    fn keys_that_name_one_format_agree_and_keys_that_name_none_defer() {
        let keys = ["a/db.json".to_owned(), "a/server.json".to_owned()];
        assert_eq!(agreed_format(&keys), Ok(Some(Format::Json)));

        // An extension nobody recognises is not a disagreement — it is a key
        // with no opinion, which `with_format` already covers.
        let keys = ["a/db.json".to_owned(), "a/server".to_owned()];
        assert_eq!(agreed_format(&keys), Ok(Some(Format::Json)));

        assert_eq!(agreed_format(&["a/db".to_owned()]), Ok(None));
        assert_eq!(agreed_format(&[]), Ok(None));
    }

    /// The confusing failure this prevents: `server.toml` parsed as JSON is a
    /// syntax error about a file with no syntax error in it.
    #[test]
    fn keys_naming_two_formats_name_both_keys_rather_than_guessing() {
        let keys = ["a/db.json".to_owned(), "a/server.toml".to_owned()];

        let complaint = agreed_format(&keys).expect_err("json and toml cannot both be it");

        assert!(complaint.contains("a/db.json"), "{complaint}");
        assert!(complaint.contains("a/server.toml"), "{complaint}");
        assert!(complaint.contains("with_format"), "{complaint}");
    }

    /// A glob metacharacter in a prefix is the concrete way this happens: a
    /// Redis `SCAN MATCH my[a]pp/*` matches `myapp/...`, which the caller
    /// never asked for.
    #[test]
    fn a_key_outside_the_prefix_is_refused() {
        under_prefix("myapp/db", "myapp/", "store").expect("this one is under it");

        let error =
            under_prefix("other/db", "myapp/", "store").expect_err("that one is not under it");

        assert!(error.to_string().contains("other/db"), "{error}");
    }

    /// Arrays are replaced whole rather than concatenated — the same thing a
    /// later file means by supplying a list, and the thing most likely to be
    /// assumed otherwise.
    #[test]
    fn a_later_key_replaces_a_list_rather_than_appending_to_it() {
        let documents = documents(&[
            ("myapp/base", r#"{"db": {"hosts": ["a", "b"]}}"#),
            ("myapp/local", r#"{"db": {"hosts": ["c"]}}"#),
        ]);

        let fetched = merged(&documents, Format::Json, Overlap::LaterWins, "store")
            .expect("two documents merge");
        let tree = Value::parse(&fetched.text, Format::Json).expect("the result is a document");

        assert_eq!(
            tree.get("db.hosts"),
            Some(&Value::Array(vec![Value::String("c".to_owned())]))
        );
    }
}
