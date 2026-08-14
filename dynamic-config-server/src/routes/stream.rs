//! The change stream: `text/event-stream`, one event per install.
//!
//! An event is a *number*. That one decision is what makes resumption a
//! comparison rather than a buffer, memory flat per connection, and
//! backpressure a non-question — the reasoning is in the crate's own
//! documentation and in the book.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use dynamic_config::Changes;
use futures_core::Stream;

use crate::document::Document;
use crate::server::{Section, Server, StreamPermit};

use crate::audit::Outcome;

use super::admit::{admit, at_capacity, refuse, served};

/// How long a stream may be silent before a comment goes down it.
///
/// Not a configuration key: it is the interval an idle TCP connection needs
/// to survive the proxies these deployments sit behind, and a deployment
/// that wants a different one has an idle timeout of its own to set.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `GET /{application}/{profile}/stream` — one `text/event-stream` per
/// caller, one event per install.
///
/// # What an event carries, and what it deliberately does not
///
/// A generation number, and the application and profile it belongs to:
///
/// ```text
/// id: 7
/// event: generation
/// data: {"application":"billing","profile":"prod","generation":7}
/// ```
///
/// **Not the document, and not the changed paths either.** The document
/// endpoint is the one endpoint that serves values, and a stream that
/// carried them would be a second one — with a different lifetime, a
/// different failure mode, and a body that outlives the request that
/// authorised it. Changed *paths* would leak nothing `/paths` does not
/// already tell the same caller, but they would have to be diffed per
/// install and carried per connection, which is the memory bound this
/// design exists to avoid. So the event says *something landed, here is its
/// number*, and the client re-fetches the endpoint it was already using.
///
/// That choice is what makes the rest of it simple:
///
/// - **Resumption is a comparison, not a buffer.** A generation is
///   monotonic and the current one subsumes every one before it, so
///   `Last-Event-ID: 6` against a section at 9 is one event carrying 9 —
///   nothing was missed, because there was never anything to miss. There is
///   no ring of recent events, so there is no bound to choose and no
///   "reconnected past the end of it" case to answer.
/// - **Backpressure needs no policy.** The stream carries a level rather
///   than a log: a client that stops reading is simply not polled, and when
///   it is polled again it gets the *latest* generation. Nothing queues, so
///   nothing has to be dropped.
/// - **Memory is flat.** Per connection: one `Changes` handle (an `Arc`
///   clone and a `u64`), one registered waker, and two short strings. No
///   document, no diff, no channel. A thousand pods reconnecting after a
///   restart cost a thousand of that and one shared install.
///
/// # It is an endpoint like every other one
///
/// Authenticated, authorised against the caller's grants, and refused with
/// the same 404 as everything else — a subscription to a section the caller
/// may not read does the same work and returns the same body as a
/// subscription to a section nobody serves. The audit log records the
/// *subscription*, once, with the generation it opened at; the events after
/// it say no more than a `/status` poll would, and a line per install per
/// connection would drown the log that matters.
pub(super) async fn stream(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((application, profile)): Path<(String, String)>,
) -> Response {
    let admitted = match admit(&server, &headers, &application, &profile, "stream") {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };

    // A deployment that turns streaming off does not serve this path, and
    // says so with the body it says everything else with. Checked after
    // admission so that the answer costs an unauthorised caller exactly
    // what every other 404 costs it.
    if !server.streams_enabled() {
        return refuse(
            &server,
            "stream",
            Outcome::NotFound,
            Some(admitted.principal.name().to_owned()),
            Some((
                admitted.section.application().to_owned(),
                admitted.section.profile().to_owned(),
            )),
        );
    }

    let Some(permit) = server.open_stream() else {
        return at_capacity(&server, &admitted);
    };

    let section = Arc::clone(admitted.section);
    let generation = section.generation();

    served(&server, &admitted, generation);

    let stream = Generations {
        application: section.application().to_owned(),
        profile: section.profile().to_owned(),
        changes: section.changes(),
        resume: last_event_id(&headers),
        sent: None,
        section,
        _permit: permit,
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// `Last-Event-ID`, as the generation a client says it already has.
///
/// Anything that is not a number is *ignored* rather than refused: the
/// header is echoed back by browsers and proxies from whatever the last
/// event carried, and a reconnect that fails because something in the path
/// mangled a header is a worse failure than one extra event.
fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// One connection's stream of generations.
///
/// Everything it holds is fixed-size. That is the property the whole design
/// turns on, so it is worth naming the fields: two strings from the server's
/// own configuration, an `Arc` to the section, a `Changes` handle, the last
/// number sent, the number the client claimed, and the permit that releases
/// its place in the ceiling on drop.
struct Generations {
    application: String,
    profile: String,
    section: Arc<Section>,
    changes: Changes<Document>,
    /// The last generation this connection emitted.
    sent: Option<u64>,
    /// What the client's `Last-Event-ID` claimed, if anything.
    resume: Option<u64>,
    _permit: StreamPermit,
}

impl Generations {
    /// Whether `generation` is news to this connection.
    fn is_news(&self, generation: u64) -> bool {
        // Zero is "nothing has ever been installed here", which is not an
        // event: a section serves nothing until it has a document, and
        // `/readyz` is where that is reported.
        if generation == 0 {
            return false;
        }

        match self.sent {
            Some(sent) => generation > sent,
            // The opening event. A client that said where it was gets one
            // unless the section is exactly where it said; a client that
            // said nothing gets one either way, so that it starts knowing
            // where it stands rather than having to guess.
            //
            // *Different*, not *greater*. A generation counts installs
            // since this process started, so a restart puts the section
            // back at 1 while a reconnecting `EventSource` still sends the
            // `Last-Event-ID` the previous process gave it. Under a
            // greater-than test, a client resuming from 50 would be told
            // nothing by the new process until it had reloaded fifty times
            // — silently missing every change in between, for the life of
            // the connection. A number that is not the one the client
            // holds is news, whichever side of it it falls.
            None => match self.resume {
                Some(resumed) => generation != resumed,
                None => true,
            },
        }
    }

    fn event(&self, generation: u64) -> Event {
        let data = serde_json::json!({
            "application": self.application,
            "profile": self.profile,
            "generation": generation,
        })
        .to_string();

        // The id *is* the generation, which is what makes `Last-Event-ID`
        // resumption a comparison rather than a lookup.
        Event::default()
            .id(generation.to_string())
            .event("generation")
            .data(data)
    }
}

impl Stream for Generations {
    // Infallible: nothing between an install and an event can fail. A
    // connection ends because the client went away, which is the body being
    // dropped rather than an error travelling down it.
    type Item = Result<Event, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            let generation = this.section.generation();

            if this.is_news(generation) {
                this.sent = Some(generation);

                return Poll::Ready(Some(Ok(this.event(generation))));
            }

            // A fresh future per poll on purpose: `changed()` keeps no state
            // of its own — the generation it has seen lives in the `Changes`
            // handle and the check-register-check protocol lives in the
            // cell — so re-creating it is the same future, and it saves
            // this type from having to be self-referential.
            let changed = this.changes.changed();

            match std::pin::pin!(changed).poll(context) {
                // Something installed. Round again to read the generation it
                // became, rather than trusting the snapshot: the number the
                // event carries is the one `/status` and the document
                // endpoint report, and it comes from the same load.
                Poll::Ready(_) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
