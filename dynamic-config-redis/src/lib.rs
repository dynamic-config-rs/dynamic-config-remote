//! Read [`dynamic-config`] configuration from a Redis key.
//!
//! Redis speaks a plain request/response protocol, so this implements the
//! **blocking** [`RemoteSource`] trait: nothing here needs an async runtime,
//! and neither does using it.
//!
//! ```no_run
//! use dynamic_config_redis::Redis;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Redis) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! DbConfig::set_remote(Redis::new("redis://redis.internal:6379", "myapp/db.json")?);
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it reads
//!
//! One key, whose value is **a whole configuration document** — the same bytes
//! that would be in a config file. The format comes from the key's extension,
//! or from [`with_format`](Redis::with_format).
//!
//! A Redis hash would be the other obvious mapping — one field per setting —
//! and is deliberately not what this does. A hash cannot hold a nested table
//! without inventing a flattening convention, and a document already has one.
//!
//! # Credentials
//!
//! In the URL, which is where Redis puts them and where every deployment
//! already has them: `redis://user:password@host:6379/0`, or `rediss://` for
//! TLS — which needs this crate's `tls` feature to supply the client's rustls
//! stack. [`from_client`](Redis::from_client) takes a client the program
//! already built, for anything the URL cannot say.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::sync::Mutex;
use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSource, Watching};
use redis::Commands;

/// Redis' own client, re-exported so [`from_client`](Redis::from_client) needs
/// no direct dependency.
pub use redis::Client;

/// How long to wait for a change before looking again.
///
/// A subscription blocks until a message arrives; this is how long that block
/// lasts before the loop checks whether it has been told to stop.
const POLL_SLICE: Duration = Duration::from_millis(250);

/// A key in Redis, as a configuration source.
///
/// Not `Clone`: it holds a connection, and two clones sharing a key while each
/// opening their own would double the connections for no gain. Wrap it in an
/// `Arc` if two places need one.
pub struct Redis {
    client: Client,
    /// One connection, reused. A source that reconnected on every read would
    /// turn a refresh loop into a connection storm.
    connection: Mutex<Option<redis::Connection>>,
    key: String,
    format: Option<Format>,
    described: String,
}

impl Redis {
    /// The key `key`, on the Redis at `url`.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one needs [`with_format`](Self::with_format).
    ///
    /// # Errors
    ///
    /// If the URL cannot be parsed. **Not** if the server is unreachable: the
    /// connection is opened on the first read, so that construction stays free
    /// of I/O like every other source in this family.
    pub fn new(url: &str, key: impl Into<String>) -> Result<Self, Error> {
        // `redacted`, even here: a malformed URL still carries its password,
        // and a parse error is the error most likely to be pasted somewhere.
        let client = Client::open(url)
            .map_err(|error| Error::remote(format!("redis {}: {error}", redacted(url))))?;

        Ok(Self::build(client, key, redacted(url)))
    }

    /// Uses a client the program already has.
    ///
    /// For a caller that already talks to Redis, or one that built its client
    /// with options a URL cannot express.
    #[must_use]
    pub fn from_client(client: Client, key: impl Into<String>) -> Self {
        Self::build(client, key, "<an existing client>".to_owned())
    }

    fn build(client: Client, key: impl Into<String>, described: String) -> Self {
        let key = key.into();

        let format = Format::from_key(&key);

        Self {
            client,
            connection: Mutex::new(None),
            key,
            format,
            described,
        }
    }

    /// States the format, for a key whose name does not.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// Calls `on_change` whenever the key's value changes.
    ///
    /// Uses **keyspace notifications**: Redis publishes to
    /// `__keyspace@{db}__:{key}` when a key is written, and this subscribes to
    /// exactly that channel. Genuinely change-driven — no polling, no timer.
    ///
    /// Keyspace notifications are **off by default** in Redis. A server that
    /// has not enabled them publishes nothing, and this loop would wait
    /// forever, so it checks at start-up and reports rather than hanging:
    ///
    /// ```text
    /// CONFIG SET notify-keyspace-events KEA
    /// ```
    ///
    /// The current value is not delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters.
    ///
    /// # Errors
    ///
    /// If the subscription cannot be established, if keyspace notifications are
    /// off, or if `on_change` returns an error — which ends the watch, so a
    /// caller that wants to survive a bad document should log it and return
    /// `Ok`.
    pub fn watch<F>(&self, watching: &Watching, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        // Validated up front so a key with no format fails at `watch` rather
        // than on the first notification, hours later. The reads themselves go
        // through `fetch`, which resolves the format again.
        self.format()?;

        self.require_keyspace_notifications()?;

        // A subscription needs a connection of its own: Redis puts the
        // connection into a mode where ordinary commands are refused.
        let mut subscriber = self.client.get_connection().map_err(|error| {
            Error::remote(format!("{}: cannot subscribe: {error}", self.describe()))
        })?;

        // The database index is not readable from the client in this version,
        // so it is asked for: `CLIENT INFO` reports the connection's own, which
        // is the one the notifications will be published on. An index that
        // cannot be determined is a hard error, not a guess of `0` — a watch
        // subscribed to the wrong database is a watch that never fires, which
        // reads as "configuration stopped changing" rather than as a failure.
        let database = self.database().ok_or_else(|| {
            Error::remote(format!(
                "{}: cannot determine the database index the connection lands                  on, so the keyspace channel cannot be named",
                self.describe()
            ))
        })?;
        let channel = format!("__keyspace@{database}__:{}", self.key);

        let mut pubsub = subscriber.as_pubsub();

        pubsub.subscribe(&channel).map_err(|error| {
            Error::remote(format!("{}: cannot subscribe: {error}", self.describe()))
        })?;

        // Bounded, so `stop` is noticed without a message having to arrive.
        pubsub
            .set_read_timeout(Some(POLL_SLICE))
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

        while watching.keep_going() {
            let Ok(message) = pubsub.get_message() else {
                // A timeout, which is how this loop gets a chance to stop.
                continue;
            };

            let event: String = message.get_payload().unwrap_or_default();

            // `del` and `expired` mean the key holds nothing. No configuration
            // is not a configuration, so the running snapshot stays.
            if event == "del" || event == "expired" {
                continue;
            }

            // Through `fetch`, not `read`: fetch drops the cached connection
            // on failure, so a read that died with its socket does not leave a
            // dead connection for every later notification to trip over.
            match self.fetch() {
                Ok(document) => on_change(document)?,
                // The notification arrived and the read did not: a transient
                // failure, and the next write will notify again.
                Err(_) => continue,
            }
        }

        Ok(())
    }

    /// The database index this client's connections land on.
    ///
    /// Notifications are published per database, so subscribing to the wrong
    /// one is a watch that never fires.
    fn database(&self) -> Option<i64> {
        let mut connection = self.client.get_connection().ok()?;
        let info: String = redis::cmd("CLIENT")
            .arg("INFO")
            .query(&mut connection)
            .ok()?;

        info.split_whitespace()
            .find_map(|field| field.strip_prefix("db="))
            .and_then(|value| value.parse().ok())
    }

    /// The format, or an error naming the call that supplies one.
    fn format(&self) -> Result<Format, Error> {
        self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })
    }

    /// Reads the key, opening the connection if this is the first time.
    fn read(&self, format: Format) -> Result<Fetched, Error> {
        let mut slot = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let connection = match slot.as_mut() {
            Some(connection) => connection,
            None => {
                let opened = self
                    .client
                    .get_connection()
                    .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

                slot.insert(opened)
            }
        };

        let text: Option<String> = connection.get(&self.key).map_err(|error| {
            // The connection may be the thing that broke; drop it so the next
            // read opens a fresh one rather than reusing a dead socket.
            Error::remote(format!("{}: {error}", self.describe()))
        })?;

        let Some(text) = text else {
            return Err(Error::remote(format!(
                "{}: the key holds no value",
                self.describe()
            )));
        };

        Ok(Fetched::new(text, format))
    }

    /// Reports if the server will never publish what the watch waits for.
    fn require_keyspace_notifications(&self) -> Result<(), Error> {
        let mut connection = self
            .client
            .get_connection()
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

        let settings: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("notify-keyspace-events")
            .query(&mut connection)
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

        let value = settings.get(1).map(String::as_str).unwrap_or_default();

        // `K` is the keyspace class; without it nothing is published on the
        // channel this subscribes to, whatever else is enabled.
        if value.contains('K') {
            return Ok(());
        }

        Err(Error::remote(format!(
            "{}: keyspace notifications are off, so nothing would ever arrive; \
             `CONFIG SET notify-keyspace-events KEA` on the server",
            self.describe()
        )))
    }
}

impl RemoteSource for Redis {
    fn fetch(&self) -> Result<Fetched, Error> {
        let format = self.format()?;

        match self.read(format) {
            Ok(document) => Ok(document),
            Err(error) => {
                // A failed read may have been a dead connection; drop it so the
                // next attempt opens a fresh one.
                *self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

                Err(error)
            }
        }
    }

    fn describe(&self) -> String {
        format!("redis {} key {}", self.described, self.key)
    }
}

impl std::fmt::Debug for Redis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redis")
            .field("server", &self.described)
            .field("key", &self.key)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// A URL with its password removed, for error messages.
///
/// `redis://user:hunter2@host` in a log is a credential in a log.
fn redacted(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };

    let Some((authority, tail)) = rest.split_once('@') else {
        return url.to_owned();
    };

    let user = authority
        .split_once(':')
        .map_or(authority, |(user, _)| user);

    format!("{scheme}://{user}:***@{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_never_reaches_an_error_message() {
        assert_eq!(
            redacted("redis://app:hunter2@redis.internal:6379"),
            "redis://app:***@redis.internal:6379"
        );
    }

    #[test]
    fn a_url_with_no_credentials_is_left_alone() {
        assert_eq!(
            redacted("redis://redis.internal:6379"),
            "redis://redis.internal:6379"
        );
        assert_eq!(redacted("not a url"), "not a url");
    }

    #[test]
    fn the_format_comes_from_the_keys_extension() {
        let client = Client::open("redis://127.0.0.1:6379").unwrap();
        let source = Redis::from_client(client, "myapp/db.json");

        assert_eq!(source.format, Some(Format::Json));
    }
}
