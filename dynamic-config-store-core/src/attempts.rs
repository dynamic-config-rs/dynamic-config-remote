//! Reporting a watch loop's *failed* attempts to reach its store.
//!
//! A watch loop is the half of a store `dynamic-config` cannot see.
//! [`RemoteSink::apply`] records a delivery, so a working watch keeps
//! [`RemoteStatus`] current — but a loop whose stream broke, whose blocking
//! query is erroring or whose credential was refused delivers nothing, and
//! without this says nothing: `dynamic_config_remote_up` would report the
//! last *delivery* rather than the last *attempt*, and a store that stopped
//! answering an hour ago would look healthy until something called
//! `refresh_remote`.
//!
//! # Why a type rather than an `Option<RemoteSink>` in seven crates
//!
//! Because the seven watch loops do not agree on anything else. Their
//! signatures already differ — blocking against async, a `Watching` token or
//! a cancelled future — so a second `watch_reporting_to` method in each crate
//! would be seven new methods with seven doc comments saying the same thing.
//! What they *do* agree on is that a failure site is one line, and that the
//! line must be impossible to get wrong: [`Attempts::failed`] is infallible,
//! is a no-op when nobody asked for reporting, and cannot be given anything
//! but an error.
//!
//! # What it deliberately does not do
//!
//! It does not touch the document, the fetch count or the clock. A failed
//! attempt moves the failure streak and the last failure and nothing else,
//! so `dynamic_config_remote_last_fetch_seconds` keeps *ageing* while
//! `dynamic_config_remote_up` goes to zero — which is the pair an alert
//! wants. A failure that reset the staleness clock would hide the half of
//! the story that says how long the served document has been stale.
//!
//! [`RemoteSink::apply`]: dynamic_config::RemoteSink::apply
//! [`RemoteStatus`]: dynamic_config::RemoteStatus

use dynamic_config::{Error, RemoteSink};

/// Where a watch loop reports an attempt that came back with nothing.
///
/// Default is *nobody asked*, which is what a source built without
/// `reporting_to` carries and what makes [`failed`](Self::failed) free.
#[derive(Clone, Copy, Debug, Default)]
pub struct Attempts(Option<RemoteSink>);

impl Attempts {
    /// Reports to `sink`.
    ///
    /// A sink is `Copy` and captures its source's generation when it is
    /// taken, which is what fences a stale loop's reports away from a
    /// replacement source. Take it where the watch is wired, once.
    #[must_use]
    pub fn to(sink: RemoteSink) -> Self {
        Self(Some(sink))
    }

    /// An attempt to reach the store came back with nothing.
    ///
    /// Infallible and silent by design: a loop must never have to handle a
    /// failure to report a failure, and a loop nobody asked to report is not
    /// paying for a branch it did not want.
    pub fn failed(&self, error: &Error) {
        if let Some(sink) = &self.0 {
            sink.failed(error);
        }
    }

    /// Whether anything is listening.
    ///
    /// For a store that would otherwise build a description or clone an
    /// error only to hand it to nobody.
    #[must_use]
    pub fn is_reporting(&self) -> bool {
        self.0.is_some()
    }
}

impl From<RemoteSink> for Attempts {
    fn from(sink: RemoteSink) -> Self {
        Self::to(sink)
    }
}

impl From<Option<RemoteSink>> for Attempts {
    fn from(sink: Option<RemoteSink>) -> Self {
        Self(sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape every store's default carries: a source nobody wired a sink
    /// into reports nowhere, and calling it is not a mistake.
    #[test]
    fn reporting_to_nobody_is_a_no_op_rather_than_a_refusal() {
        let attempts = Attempts::default();

        assert!(!attempts.is_reporting());

        // Infallible, and there is nothing to unwrap or ignore.
        attempts.failed(&Error::remote("the subscription dropped"));
    }
}
