//! A stage between the store and the resolver.
//!
//! Every store here answers with a document and the engine resolves it.
//! Between those two there is room for one more thing: a document that
//! arrives **encrypted** and has to be readable before anything can parse
//! it. A YAML in git whose passwords are ciphertext is reviewable in a pull
//! request, diffable, and useless to whoever clones the repository — which
//! is the shape SOPS made popular and the reason this stage exists.
//!
//! # Why a wrapper and not an engine stage
//!
//! It could have been a new step in the engine's pipeline, and that was the
//! first design. A wrapper is better for a reason that only shows up when
//! you write both: **it composes with every store already**. `Transformed`
//! is a [`RemoteSource`] that holds a [`RemoteSource`], so decrypting a
//! document from git, from S3, from Consul or from a config server is the
//! same line of code — and the engine grows no API at all, which means an
//! application that never encrypts anything pays nothing and reads nothing
//! about it.
//!
//! ```no_run
//! # use dynamic_config_store_core::transform::{Transform, Transformed};
//! # use dynamic_config::Error;
//! # struct Rot13;
//! # impl Transform for Rot13 {
//! #     fn apply(&self, document: &str) -> Result<String, Error> { Ok(document.to_owned()) }
//! #     fn describe(&self) -> String { "rot13".to_owned() }
//! # }
//! # fn example(store: impl dynamic_config::RemoteSource + 'static) {
//! let source = Transformed::new(store, Rot13);
//! # }
//! ```
//!
//! # What passes through untouched
//!
//! The revision and the lease. A transform rewrites *what the document
//! says*, not which version of it this is or what credential it was issued
//! under — so the engine's recency fence and the agent's renewal timer go
//! on working exactly as they did, which is the property that makes this
//! safe to put in front of anything.

use dynamic_config::{Error, Fetched, RemoteSource, WatchCapability, Watching};

/// Something that rewrites a document on its way past.
///
/// Blocking, like the sources it wraps: a transform that needs the network
/// is talking to one more service on the same thread the fetch already
/// runs on, and its caller already arranges for that.
pub trait Transform: Send + Sync + 'static {
    /// The document, readable.
    ///
    /// # Errors
    ///
    /// If it cannot be. **The message must not carry the document** — a
    /// failure to decrypt is about a key or a format, and quoting what
    /// failed would put the ciphertext, and sometimes the plaintext, in a
    /// log.
    fn apply(&self, document: &str) -> Result<String, Error>;

    /// For diagnostics: what this stage is, never what it holds.
    fn describe(&self) -> String;
}

/// A store with a [`Transform`] in front of it.
///
/// Reads exactly like the store it wraps, because it is one — the same
/// watch, the same capability, the same revision — with the document
/// rewritten between the store answering and the engine parsing.
pub struct Transformed<S, T> {
    source: S,
    transform: T,
}

impl<S, T> Transformed<S, T> {
    /// Puts `transform` in front of `source`.
    pub const fn new(source: S, transform: T) -> Self {
        Self { source, transform }
    }
}

impl<S: RemoteSource, T: Transform> Transformed<S, T> {
    /// The document, transformed, with everything else carried across.
    fn rewrite(&self, fetched: Fetched) -> Result<Fetched, Error> {
        let text = self.transform.apply(&fetched.text)?;

        // Rebuilt rather than mutated because `Fetched` is
        // `#[non_exhaustive]`, and carried across field by field so that a
        // field added later is a compile error here rather than a silently
        // dropped revision.
        let mut carried = Fetched::new(text, fetched.format);

        if let Some(revision) = fetched.revision {
            carried = carried.with_revision(revision);
        }

        if let Some(lease) = fetched.lease {
            carried = carried.with_lease(lease);
        }

        Ok(carried)
    }
}

impl<S: RemoteSource, T: Transform> RemoteSource for Transformed<S, T> {
    fn fetch(&self) -> Result<Fetched, Error> {
        let fetched = self.source.fetch()?;

        self.rewrite(fetched)
    }

    fn describe(&self) -> String {
        format!(
            "{} through {}",
            self.source.describe(),
            self.transform.describe()
        )
    }

    fn watch_capability(&self) -> WatchCapability {
        self.source.watch_capability()
    }

    fn watch(
        &self,
        watching: &Watching,
        interval: std::time::Duration,
        on_change: &mut dyn FnMut(Fetched) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // The inner store's own watch, with the transform applied to each
        // delivery. A store that pushes goes on pushing; the callback is
        // the only thing between it and the caller.
        //
        // A transform that fails on a delivery **ends the watch** rather
        // than dropping the document silently: a key that stopped working
        // is not a change nobody needs to hear about.
        self.source.watch(watching, interval, &mut |fetched| {
            let rewritten = self.rewrite(fetched)?;

            on_change(rewritten)
        })
    }
}

impl<S, T> std::fmt::Debug for Transformed<S, T>
where
    S: RemoteSource,
    T: Transform,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transformed")
            .field("source", &self.source.describe())
            .field("transform", &self.transform.describe())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamic_config::Format;

    struct Ciphertext;

    impl RemoteSource for Ciphertext {
        fn fetch(&self) -> Result<Fetched, Error> {
            Ok(
                Fetched::new("{\"password\":\"SEALED\"}".to_owned(), Format::Json)
                    .with_revision(dynamic_config::Revision::Counter(7)),
            )
        }

        fn describe(&self) -> String {
            "a sealed store".to_owned()
        }
    }

    struct Unseal;

    impl Transform for Unseal {
        fn apply(&self, document: &str) -> Result<String, Error> {
            Ok(document.replace("SEALED", "hunter2"))
        }

        fn describe(&self) -> String {
            "unseal".to_owned()
        }
    }

    struct Broken;

    impl Transform for Broken {
        fn apply(&self, _document: &str) -> Result<String, Error> {
            Err(Error::remote("the key is not available"))
        }

        fn describe(&self) -> String {
            "broken".to_owned()
        }
    }

    #[test]
    fn the_document_is_rewritten_and_everything_else_is_carried() {
        let source = Transformed::new(Ciphertext, Unseal);
        let fetched = source.fetch().expect("a document");

        assert!(fetched.text.contains("hunter2"));
        assert_eq!(fetched.format, Format::Json);

        // The revision is what the engine's recency fence compares, so
        // losing it here would let an older document install over a newer
        // one — silently, and only under a transform.
        assert_eq!(
            fetched.revision,
            Some(dynamic_config::Revision::Counter(7)),
            "the store's revision must survive the transform"
        );
    }

    #[test]
    fn a_transform_that_fails_fails_the_fetch() {
        let source = Transformed::new(Ciphertext, Broken);

        let error = source.fetch().expect_err("the key is not available");

        assert!(error.to_string().contains("not available"));
    }

    /// A diagnostic names both halves, so "which store?" and "through
    /// what?" are one line rather than two guesses.
    #[test]
    fn the_description_names_both_halves() {
        let source = Transformed::new(Ciphertext, Unseal);

        assert_eq!(source.describe(), "a sealed store through unseal");
    }

    /// Never the document. A failure to decrypt is about a key or a format,
    /// and quoting what failed would put ciphertext — and sometimes
    /// plaintext — in a log.
    #[test]
    fn a_failure_carries_no_document() {
        let source = Transformed::new(Ciphertext, Broken);
        let error = source.fetch().expect_err("broken").to_string();

        assert!(!error.contains("SEALED"), "{error}");
        assert!(!error.contains("password"), "{error}");
    }
}
