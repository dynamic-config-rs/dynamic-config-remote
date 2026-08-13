//! Who read what, when — and never what was in it.
//!
//! The library's rule is that a value never reaches a diagnostic. A server
//! makes that rule harder to keep and more important to keep: it is the one
//! program here that *serves* values, so its log is the obvious place for
//! one to escape on the way past.
//!
//! The answer is structural rather than careful. An [`AuditEntry`] has no
//! field a configuration value could occupy: a caller name and an endpoint,
//! both from the server's own configuration or from a fixed list, an
//! application and a profile that have already passed the request-shape
//! check, an outcome and a generation number. There is nowhere to put a
//! value, so no amount of future editing puts one there.

use std::fmt;

/// What happened to one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The caller was authorised and got what it asked for.
    Served,
    /// The caller presented no usable credential.
    Unauthenticated,
    /// The caller is somebody, but not somebody who may read this — or
    /// nothing is served here. **One outcome for both**, because the server
    /// does not distinguish them to the caller and an audit log that did
    /// would be a way to ask it to.
    NotFound,
    /// The request's shape was refused before anything was looked up.
    Malformed,
    /// The caller was authorised and the section could not answer: it has
    /// no document yet, or a diagnostic could not read the sources. About
    /// the server, not about the caller — which is why it is not
    /// [`NotFound`](Self::NotFound).
    Unavailable,
}

impl Outcome {
    /// A short, stable label, for a log field or a metric dimension.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Unauthenticated => "unauthenticated",
            Self::NotFound => "not-found",
            Self::Malformed => "malformed",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One line of the audit log.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AuditEntry {
    /// The configured client name, or `None` when nobody was identified.
    pub caller: Option<String>,
    /// The application asked for — `None` when the request shape was
    /// refused, because an unvalidated path segment is attacker-controlled
    /// text and a log line is a place newlines matter.
    pub application: Option<String>,
    /// The profile asked for, under the same rule.
    pub profile: Option<String>,
    /// Which endpoint, from a fixed list.
    pub endpoint: &'static str,
    /// How it ended.
    pub outcome: Outcome,
    /// The generation served, when something was.
    pub generation: Option<u64>,
}

impl fmt::Display for AuditEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn field(value: Option<&String>) -> &str {
            value.map_or("-", String::as_str)
        }

        write!(
            f,
            "audit caller={} application={} profile={} endpoint={} outcome={} generation={}",
            field(self.caller.as_ref()),
            field(self.application.as_ref()),
            field(self.profile.as_ref()),
            self.endpoint,
            self.outcome.as_str(),
            self.generation
                .map_or_else(|| "-".to_owned(), |it| it.to_string()),
        )
    }
}

/// Where audit lines go.
///
/// A trait rather than a `tracing` call, for two reasons that point the same
/// way: a deployment's audit trail usually belongs somewhere other than
/// stderr, and a test that asserts *no value ever appears in the log* needs
/// the log in a `Vec` it can read.
pub trait AuditSink: Send + Sync + 'static {
    /// Records one request.
    ///
    /// Called on the request's own task, after the response is decided and
    /// before it is returned. A sink that blocks blocks a request, so a sink
    /// that talks to a network should hand off to a queue.
    fn record(&self, entry: &AuditEntry);
}

/// The default sink: one line per request on stderr.
#[derive(Debug, Clone, Copy, Default)]
pub struct StderrAudit;

impl AuditSink for StderrAudit {
    fn record(&self, entry: &AuditEntry) {
        eprintln!("{entry}");
    }
}

/// A sink that records nothing.
///
/// For a deployment that audits in front of this server instead, and for
/// tests that are not about the log.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAudit;

impl AuditSink for NoAudit {
    fn record(&self, _entry: &AuditEntry) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_renders_every_field_and_dashes_the_absent_ones() {
        let entry = AuditEntry {
            caller: Some("billing-pod".to_owned()),
            application: Some("billing".to_owned()),
            profile: Some("prod".to_owned()),
            endpoint: "document",
            outcome: Outcome::Served,
            generation: Some(3),
        };

        assert_eq!(
            entry.to_string(),
            "audit caller=billing-pod application=billing profile=prod endpoint=document \
             outcome=served generation=3"
        );

        let entry = AuditEntry {
            caller: None,
            application: None,
            profile: None,
            endpoint: "document",
            outcome: Outcome::Unauthenticated,
            generation: None,
        };

        assert_eq!(
            entry.to_string(),
            "audit caller=- application=- profile=- endpoint=document \
             outcome=unauthenticated generation=-"
        );
    }
}
