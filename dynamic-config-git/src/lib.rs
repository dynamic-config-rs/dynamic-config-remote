//! Read [`dynamic-config`] configuration from a git repository.
//!
//! Configuration in git is how a great many teams already work: review,
//! history, blame and rollback come free, and nobody runs etcd for a file that
//! changes twice a month. This crate reads a file — or a set of them, out of
//! one commit — at one ref, from one repository, and hands it to
//! `dynamic-config` the way every other store crate does.
//!
//! ```no_run
//! use dynamic_config_git::{Credential, GitSource};
//!
//! # struct AppConfigBuilder;
//! # impl AppConfigBuilder { fn init(&self) -> Result<(), dynamic_config::Error> { Ok(()) } }
//! # struct AppConfig;
//! # impl AppConfig {
//! #     fn set_remote(_: GitSource) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! #     fn builder(_: &str) -> AppConfigBuilder { AppConfigBuilder }
//! # }
//! let source = GitSource::builder("https://github.com/acme/config.git")
//!     .branch("main")
//!     .path("services/api/config.yaml")
//!     .credential(Credential::token(std::env::var("GITHUB_TOKEN")?))
//!     .build()?;
//!
//! AppConfig::set_remote(source);
//!
//! // Fetching is explicit; the load that follows touches no network.
//! AppConfig::refresh_remote()?;
//! AppConfig::builder("app").init()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Why git rather than four REST APIs
//!
//! GitHub, GitLab, Azure DevOps, Gitea, Bitbucket and a bare
//! `git@host:repo.git` **all speak git**. Their file APIs are five clients,
//! five auth models, five pagination stories and five ways of spelling *this
//! ref*. "Compatible with all of them" is only reachable through the protocol
//! they share, and the extra round trip that protocol costs is irrelevant at
//! configuration cadence.
//!
//! The implementation is [`gix`] — pure Rust, no `libgit2`, no C toolchain and
//! no OpenSSL question. The exception is SSH, which `gix` carries by spawning
//! the system `ssh` exactly as `git` does; see [`SshAuth`].
//!
//! # What a fetch actually does
//!
//! **A shallow, single-ref fetch into a bare object database — never a
//! clone.** In order:
//!
//! 1. Connect, shake hands, and read the ref advertisement. This is what
//!    `git ls-remote` costs: a few hundred bytes, and no objects.
//! 2. If the commit the ref names is already in the object database, stop.
//!    **An unchanged ref transfers nothing.** That is what makes polling a git
//!    host reasonable.
//! 3. Otherwise ask for that one commit at depth 1 — the commit and its trees
//!    and blobs, and none of the history behind it.
//! 4. Read one blob out of the tree, in memory. Nothing is ever checked out.
//!
//! What it costs: the first fetch transfers the repository's current tree —
//! every file at that commit, not just the one asked for, because a commit's
//! tree is what the protocol delivers. A monorepo whose tree is a gigabyte
//! will transfer a gigabyte once. Subsequent fetches transfer one commit's
//! worth of changes.
//!
//! Filtering by path would cut that first transfer to the files actually read,
//! and **it is not implemented because nothing below this crate can express
//! it**, which is worth being precise about rather than calling it a
//! to-do. `gix` 0.86 exposes no filter on a fetch: the protocol argument
//! exists one layer down in `gix-protocol`, on a type only `gix`'s own fetch
//! ever holds. And the filter the large hosts actually serve is `blob:none`,
//! which answers with a tree whose blobs are *absent* — reading one then means
//! a lazy fetch from a promisor remote, which nothing in this dependency graph
//! implements. A path filter is therefore two upstream features away, not one
//! call. The honest summary is that this crate is comfortable with a
//! configuration repository and will be slow to start against a monorepo.
//!
//! There is no working tree, and that is the security decision as much as the
//! performance one: a repository whose tree contains a symlink to
//! `/etc/shadow`, or an entry named `../../etc/shadow`, cannot make a checkout
//! that never happens write anywhere. See [`Builder::path`].
//!
//! # Which ref, and why a branch is the default
//!
//! [`Reference`] is a branch, a tag or a commit SHA, and all three are
//! legitimate:
//!
//! | | Moves | Reproducible | For |
//! |---|---|---|---|
//! | [`branch`](Builder::branch) — the default, `main` | yes | no | hot reload: a merge to `main` *is* the deployment |
//! | [`tag`](Builder::tag) | only if force-pushed | nearly | a release train |
//! | [`commit`](Builder::commit) | never | yes | pinning a fleet to a known configuration |
//!
//! A branch is the default because a configuration store's reason to exist is
//! that the configuration changes: pinning a SHA and then starting a watcher
//! is asking a loop to wait for something that cannot happen. Pin the SHA when
//! reproducibility matters more than reload, and say so by writing it down.
//!
//! A SHA is fetched by asking the host for that object directly. Hosts that
//! allow it — GitHub, GitLab and Azure DevOps do — answer; one that has
//! `uploadpack.allowReachableSHA1InWant` off will refuse, and the error says
//! so.
//!
//! # Where the objects live
//!
//! A private directory, `0700` from the moment it exists. By default a
//! temporary one, removed with the source; name your own with
//! [`cache_dir`](Builder::cache_dir) to survive restarts. The trade-offs, and
//! why two sources may not share one, are in [`working`](mod@working).
//!
//! # When a fetch fails
//!
//! It does not take the program down. A failed [`RemoteSource::fetch`] leaves
//! the previously fetched document installed and the previously loaded
//! configuration serving — that is `dynamic-config`'s last-known-good
//! machinery, and this crate's only job is to report accurately enough for it
//! to work:
//!
//! - a host that refuses the credential is
//!   [`ErrorKind::Auth`](dynamic_config::ErrorKind::Auth), because waiting will
//!   not fix a wrong token and a watch loop should stop rather than hammer;
//! - everything else — an unreachable host, a ref that does not exist, a
//!   document that is not UTF-8 — is
//!   [`ErrorKind::Remote`](dynamic_config::ErrorKind::Remote), which a watch
//!   loop waits out.
//!
//! # Credentials never appear in a diagnostic
//!
//! A git remote URL routinely embeds one. Every error message, every `Debug`
//! and every string this crate produces puts the URL through
//! [`dynamic_config_store_core::redacted`] first, and the tests plant a token
//! and assert it is absent. An SSH key's contents are never read by this crate
//! at all, and a passphrase is never accepted — see [`auth`](mod@auth) for why.
//!
//! # Watching
//!
//! git has no watch, so [`GitSource::watch`] polls — and says so. Each tick is
//! one ref advertisement; only a ref that moved costs a transfer. The push
//! half needs nothing from this crate: whoever terminates a GitHub or GitLab
//! webhook calls the generated `remote_sink().apply(..)`.
//!
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_git::GitSource;
//! # use std::time::Duration;
//! # struct Sink;
//! # impl Sink {
//! #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # fn example(source: GitSource) {
//! # let sink = Sink;
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || {
//!     source.watch(&watching, Duration::from_secs(60), move |document| sink.apply(document))
//! });
//!
//! // Dropping `watch` — or calling `watch.stop()` — ends the loop.
//! # }
//! ```
//!
//! # Several files as one document
//!
//! One repository, one ref, and either one path, a list of them or a directory
//! — see [`Keys`]. A fetch resolves **one commit**, and a commit has **one
//! tree**, so a set of files is read as of one instant with nothing arranged
//! for it: no transaction, no listing race, no second round trip. That is why
//! this is the only store in the family whose multi-file sources can also be
//! [watched](GitSource::watch); the others refuse, and say why.
//!
//! # A host this machine does not already trust
//!
//! An enterprise GitLab behind a private certificate authority, or a host that
//! wants a client certificate first, is [`Builder::tls`]. It is the `https://`
//! knob and only that one — an `ssh://` remote's trust lives in `known_hosts`
//! and its client identity in a key, which is [`Credential::ssh_agent`],
//! [`Credential::ssh_key`] or [`Credential::ssh_command`], and asking for both
//! is refused rather than half-applied. There is no way to turn verification
//! off; [`tls`](mod@tls) has the measurement and the argument.
//!
//! # What this crate deliberately does not do
//!
//! **An async implementation.** A git fetch is blocking work — negotiation,
//! decompression, index writing — so this implements the blocking
//! [`RemoteSource`]. An async program loses nothing: `refresh_remote_async()`
//! puts a blocking source on `dynamic_config::off_thread`, so the executor's
//! worker never sits inside it.
//!
//! **Shelling out to the system `git`.** It would reach every credential
//! helper on the host for free, and it would also be a second implementation
//! of every decision on this page — the fetch shape, the ref pinning, the
//! error classification, and the redaction of a URL that `git` prints into its
//! own stderr. [`SshAuth::Command`] reaches the one method the pure-Rust path
//! cannot, and does it without a second code path.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`gix`]: https://docs.rs/gix

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSource, Watching};
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::guarded;

pub mod auth;
mod fetch;
pub mod tls;
mod url;
pub mod working;

pub use auth::{Auth, Credential, SshAuth};
// Re-exported rather than mirrored: it is one TLS vocabulary for every store
// crate in this family, and a caller configuring two stores should write the
// same three calls for both.
pub use dynamic_config_store_core::tls::TlsConfig;

use auth::Session;
use fetch::Failure;
use url::redacted;
use working::Working;

/// How long one fetch may take. Thirty seconds, because a git fetch is a
/// negotiation and a decompression rather than one HTTP GET.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How large a configuration file may be before it is refused, in bytes.
///
/// A megabyte is enormous for configuration and small enough that a hostile or
/// mistaken repository cannot make this process allocate its way out of
/// memory. Raise it with [`Builder::max_bytes`] if a real file needs it.
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// The default branch, when none is named.
const DEFAULT_BRANCH: &str = "main";

/// Where the fetched ref is written locally.
///
/// Under `refs/` but outside `refs/heads` and `refs/remotes`, so nothing
/// mistakes this working directory for a checkout somebody might want to use.
const LOCAL_REF: &str = "refs/dynamic-config/head";

/// Which commit to read.
///
/// See the [crate documentation](crate#which-ref-and-why-a-branch-is-the-default)
/// for which to choose.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reference {
    /// A branch, by short name — `main`, not `refs/heads/main`.
    Branch(String),
    /// A tag, by short name.
    Tag(String),
    /// A commit, by full hexadecimal object id.
    Commit(String),
}

impl Reference {
    /// The refspec that fetches this reference into [`LOCAL_REF`].
    fn refspec(&self) -> String {
        let source = match self {
            Self::Branch(name) => format!("refs/heads/{name}"),
            Self::Tag(name) => format!("refs/tags/{name}"),
            Self::Commit(sha) => sha.clone(),
        };

        format!("+{source}:{LOCAL_REF}")
    }

    /// The full ref name a host would advertise for this reference.
    fn advertised(&self) -> Option<String> {
        match self {
            Self::Branch(name) => Some(format!("refs/heads/{name}")),
            Self::Tag(name) => Some(format!("refs/tags/{name}")),
            Self::Commit(_) => None,
        }
    }

    /// Picks this reference's commit out of what the host advertised.
    ///
    /// By name rather than by position: a host advertises everything the
    /// refspec matched, and taking "the first one" would silently read a
    /// different branch the day a refspec grows a wildcard.
    fn resolve(
        &self,
        ref_map: &gix::remote::fetch::RefMap,
        url: &str,
    ) -> Result<gix::ObjectId, Failure> {
        let wanted = self.advertised();

        let found = ref_map
            .mappings
            .iter()
            .find(|mapping| match &mapping.remote {
                gix::remote::fetch::refmap::Source::ObjectId(_) => wanted.is_none(),
                gix::remote::fetch::refmap::Source::Ref(remote) => wanted
                    .as_deref()
                    .is_some_and(|wanted| remote.unpack().0 == wanted),
            });

        found
            .and_then(|mapping| mapping.remote.as_id())
            .map(gix::hash::oid::to_owned)
            .ok_or_else(|| {
                Failure::Other(Error::remote(format!(
                    "git {}: there is no {self} on that remote",
                    redacted(url)
                )))
            })
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Branch(name) => write!(f, "branch {name}"),
            Self::Tag(name) => write!(f, "tag {name}"),
            Self::Commit(sha) => write!(f, "commit {sha}"),
        }
    }
}

/// What a source reads: one file, several named ones, or a directory.
///
/// [`Builder::path`] takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-file spelling every caller already wrote keeps
/// working unchanged.
///
/// # Why this store can do it and the key-value ones cannot
///
/// Every store in this family folds several keys into one document, and every
/// other one has to say out loud that the set is **not** read at one instant:
/// S3 issues one `GetObject` per key, Vault one read per path, Consul one
/// request per key. A deployment writing two of them is a document that never
/// existed.
///
/// A git fetch resolves **one commit**, and a commit has **one tree**. Every
/// path below is read out of that tree, so the set is atomic with no
/// transaction, no listing race and no second round trip — the guarantee comes
/// from git's object model rather than from anything this crate arranges. That
/// is also why a multi-file git source **can be watched**, which no other store
/// here allows: see [`GitSource::watch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keys {
    /// One file, whose contents are the whole document.
    ///
    /// Handed to the loader byte for byte — never parsed and re-rendered, so
    /// comments and key order survive and no format feature is needed that was
    /// not needed before.
    One(String),

    /// Several named files, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. Tables merge deeply and arrays
    /// are replaced whole.
    Several(Vec<String>),

    /// Every file under a directory, merged as **disjoint sections**.
    ///
    /// A caller naming a directory is not expressing an order — a tree lists
    /// its entries in the order git sorted them, which is nobody's precedence
    /// — so two files under it supplying the same path is a deployment bug,
    /// and reported as one rather than resolved.
    ///
    /// A **directory**, not a string prefix, because a git tree has
    /// directories: `Keys::prefix("services/api")` reads `services/api/db.yaml`
    /// and does not read `services/api-old.yaml`. The walk is recursive, an
    /// empty string is the repository root, and every file found has to parse —
    /// so point it at a directory that holds configuration and nothing else.
    Prefix(String),
}

impl Keys {
    /// One file, whose contents are the whole document.
    #[must_use]
    pub fn one(path: impl Into<String>) -> Self {
        Self::One(path.into())
    }

    /// Several named files, merged in the order given — later wins.
    #[must_use]
    pub fn several<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Several(paths.into_iter().map(Into::into).collect())
    }

    /// Every file under `directory`, merged as disjoint sections.
    #[must_use]
    pub fn prefix(directory: impl Into<String>) -> Self {
        Self::Prefix(directory.into())
    }

    /// The paths as a slice, for the checks and the format inference.
    ///
    /// A directory has none to list — the set is not known until a commit has
    /// been read.
    fn named(&self) -> &[String] {
        match self {
            Self::One(path) => std::slice::from_ref(path),
            Self::Several(paths) => paths,
            Self::Prefix(_) => &[],
        }
    }

    /// How a diagnostic names what this source reads.
    ///
    /// One path renders as the path itself, so every message a single-file
    /// source has ever produced is unchanged.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::One(path) => path.clone(),
            Self::Several(paths) => format!("paths {}", paths.join(", ")),
            Self::Prefix(directory) => format!("everything under {directory:?}"),
        }
    }

    /// What two of this source's files supplying one path means.
    ///
    /// The distinction the whole feature turns on: a caller who wrote the list
    /// wrote the precedence with it, and a caller who named a directory wrote
    /// no order at all — so the first merges and the second refuses.
    fn overlap(&self) -> Overlap {
        match self {
            Self::One(_) | Self::Several(_) => Overlap::LaterWins,
            Self::Prefix(_) => Overlap::Refused,
        }
    }
}

impl From<&str> for Keys {
    fn from(path: &str) -> Self {
        Self::one(path)
    }
}

impl From<String> for Keys {
    fn from(path: String) -> Self {
        Self::One(path)
    }
}

impl From<&String> for Keys {
    fn from(path: &String) -> Self {
        Self::one(path)
    }
}

/// A file in a git repository, as a configuration source.
///
/// Built with [`GitSource::builder`]. Not `Clone`: the working directory is
/// claimed by one source, and two clones fetching into it would interleave
/// their ref updates. Wrap it in an `Arc` if two places need one.
pub struct GitSource {
    url: String,
    reference: Reference,
    keys: Keys,
    format: Format,
    credential: Session,
    tls: TlsConfig,
    working: Working,
    timeout: Duration,
    max_bytes: u64,
    /// Transfers a working directory may accumulate before it is emptied;
    /// `0` never empties it. Only ever a directory this crate created.
    compact_after: u32,
    /// The commit last read, for provenance in [`RemoteSource::describe`].
    last: Mutex<Option<gix::ObjectId>>,
    /// Held across a fetch. `gix::Repository` is opened per fetch rather than
    /// kept, so this is what stops two threads writing one object database —
    /// and it is on the fetch path, never on `load()`'s.
    fetching: Mutex<()>,
}

impl GitSource {
    /// A source reading from the repository at `url`.
    ///
    /// `url` is anything git understands: `https://…`, `ssh://…`,
    /// `git@host:org/repo.git`, or a local path.
    pub fn builder(url: impl Into<String>) -> Builder {
        Builder {
            url: url.into(),
            reference: Reference::Branch(DEFAULT_BRANCH.to_owned()),
            path: None,
            format: None,
            credential: Credential::anonymous(),
            tls: TlsConfig::new(),
            cache_dir: None,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
            compact_after: working::AFTER,
        }
    }

    /// Calls `on_change` when the ref moves, checking every `interval`.
    ///
    /// Polling, because git offers nothing better — and *advertisement*
    /// polling, because transferring a commit every tick to discover it has
    /// not changed would be a poor thing to do to a git host. Each tick is one
    /// handshake and one ref advertisement; only a ref that moved costs a
    /// transfer.
    ///
    /// The current value is **not** delivered at startup, for the same reason
    /// a file watcher does not report an edit when it starts. Fetch first if
    /// the starting value matters, which it usually does:
    ///
    /// ```no_run
    /// # use dynamic_config::{RemoteSource, RemoteWatch};
    /// # use dynamic_config_git::GitSource;
    /// # use std::time::Duration;
    /// # struct Sink;
    /// # impl Sink {
    /// #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # fn example(source: GitSource, watching: dynamic_config::Watching) -> Result<(), dynamic_config::Error> {
    /// # let sink = Sink;
    /// sink.apply(source.fetch()?)?;
    /// source.watch(&watching, Duration::from_secs(60), move |document| sink.apply(document))
    /// # }
    /// ```
    ///
    /// A host that is away, a ref that has been deleted, a document that does
    /// not parse — none of those end the watch. It waits out the interval and
    /// tries again. `stop` is noticed within a quarter second regardless of how
    /// long `interval` is.
    ///
    /// # A source reading several files can be watched
    ///
    /// It is the only one in this family that can. Every other store refuses a
    /// watch on a set, and the reason is written down: waking on a change to
    /// one key and then re-reading key by key collects the new value of that
    /// key and whatever the others happen to be halfway through a deployment —
    /// a document that never existed at any instant, installed and then served
    /// until the next change.
    ///
    /// Neither half of that applies here. What moves is a **ref**, and what a
    /// ref names is a **commit** — so the watch does not wake on one file, it
    /// wakes on the repository, and the re-read that follows takes every file
    /// out of that one commit's tree. A deployment that writes four files in one
    /// commit is delivered as one document; a deployment that writes them in
    /// four commits is delivered as up to four documents, each of which is a
    /// state the repository really was in. There is no interleaving to be had.
    ///
    /// The cost is the other direction, and it is the same cost a single-file
    /// watch has always had: a commit that touches nothing this source reads
    /// still moves the ref, so `on_change` is called with a document identical
    /// to the last one. A spurious delivery, never a torn one — `dynamic-config`
    /// diffs it and reports no changes.
    ///
    /// # Errors
    ///
    /// If the host refuses a credential that cannot be replaced — a token
    /// handed in as a constant is the same token next tick, so retrying it
    /// forever would be a hot loop against a host that may well start locking
    /// the account. A credential that came from a closure is refreshed and
    /// retried instead, and only ends the watch if the fresh one is refused
    /// too. Or if `on_change` returns an error, which ends the watch — so a
    /// caller that wants to survive a bad document should log it and return
    /// `Ok`.
    pub fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        let mut seen: Option<gix::ObjectId> = None;

        while watching.keep_going() {
            match self.attempt(false) {
                // The first tick records where the ref is without firing: the
                // commit it names is the one the caller already has.
                Ok(current) if seen.is_none() => seen = Some(current.commit),

                Ok(current) if seen != Some(current.commit) => {
                    // Read through `attempt` again rather than reusing the
                    // advertisement: the commit is taken from the read itself,
                    // so a push landing between the check and the read is
                    // delivered once rather than now and again next tick.
                    if let Ok((document, commit)) = self.read() {
                        seen = Some(commit);

                        guarded(&mut on_change, document, &self.describe())?;
                    }
                }

                // A credential nothing can replace: the next tick would present
                // the identical string and be refused identically.
                Err(error)
                    if error.kind() == dynamic_config::ErrorKind::Auth
                        && !self.credential.is_replaceable() =>
                {
                    return Err(error)
                }

                // Unchanged, or a failure the next tick may not have.
                _ => {}
            }

            watching.sleep_for(interval);
        }

        Ok(())
    }

    /// The document, and the commit it was read at.
    ///
    /// The fold from several files into one document happens here rather than
    /// in [`fetch`](mod@fetch), because it is the same fold every store crate
    /// in this family performs and it lives in `dynamic-config-store-core`.
    fn read(&self) -> Result<(Fetched, gix::ObjectId), Error> {
        let found = self.attempt(true)?;

        // Before `describe()`, so a diagnostic from the merge names the commit
        // the documents actually came from rather than the ref they were asked
        // for.
        *self.last() = Some(found.commit);

        let document = documents::merged(
            &found.documents,
            self.format,
            self.keys.overlap(),
            &self.describe(),
        )?;

        Ok((document, found.commit))
    }

    /// One fetch, with one retry if the credential turned out to be dead.
    fn attempt(&self, want_document: bool) -> Result<Found, Error> {
        match self.once(want_document) {
            Err(Failure::Refused(_)) if self.credential.is_replaceable() => {
                // The proactive refresh should have caught an expiring token,
                // but clocks skew and an installation can be revoked. One
                // fresh credential and one retry — not a loop: if the new one
                // is refused too, the grant is wrong and retrying would turn a
                // clear failure into a hang.
                self.credential.invalidate();

                self.once(want_document).map_err(Failure::into_error)
            }
            outcome => outcome.map_err(Failure::into_error),
        }
    }

    fn once(&self, want_document: bool) -> Result<Found, Failure> {
        // `Other`, not `Refused`: a closure that could not produce a
        // credential has nothing to be replaced by, and the retry `Refused`
        // triggers would call the same closure again in the same breath. The
        // error keeps whatever kind the closure gave it.
        let auth = self.credential.current().map_err(Failure::Other)?;

        let _fetching = self
            .fetching
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let directory = self.working.path().map_err(Failure::Other)?;

        // Before opening, not after: `compact` empties the directory, and
        // what it held is rebuilt by the `init_bare` inside `open`. A shallow
        // fetch of a moving branch adds a pack every time the branch moves and
        // removes nothing, so without this a long-lived watcher grows without
        // bound. It only ever touches a directory this crate created — see
        // `working`'s module documentation for the rule.
        working::compact(directory, self.compact_after).map_err(Failure::Other)?;

        let repository = fetch::open(directory, auth.ssh_command())?;

        let commit = fetch::fetch(
            &repository,
            &fetch::Plan {
                url: &self.url,
                reference: &self.reference,
                auth: &auth,
                tls: &self.tls,
                timeout: self.timeout,
                described: &self.describe(),
            },
            want_document,
        )?;

        let documents = if want_document {
            fetch::read_documents(&repository, commit, &self.keys, self.max_bytes, &self.url)?
        } else {
            // The watch's idle check only needs to know whether the ref moved.
            Vec::new()
        };

        Ok(Found { commit, documents })
    }

    fn last(&self) -> std::sync::MutexGuard<'_, Option<gix::ObjectId>> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What one attempt produced: the commit, and every document read out of it.
struct Found {
    commit: gix::ObjectId,
    /// `(path, contents)` in merge order. Empty when only the commit was
    /// wanted.
    documents: Vec<(String, String)>,
}

impl RemoteSource for GitSource {
    fn fetch(&self) -> Result<Fetched, Error> {
        self.read().map(|(document, _commit)| document)
    }

    fn describe(&self) -> String {
        // The commit too, once one has been read: "which commit is this
        // program actually serving" is the first question of every
        // configuration-in-git incident, and a branch name does not answer it.
        match *self.last() {
            Some(commit) => format!(
                "git {}@{}:{}",
                redacted(&self.url),
                commit.to_hex_with_len(12),
                self.keys.describe()
            ),
            None => format!(
                "git {} {}:{}",
                redacted(&self.url),
                self.reference,
                self.keys.describe()
            ),
        }
    }
}

// Hand-written, never derived: a derive would print every field, and the URL
// is a field that routinely carries a token. `{:?}` reaching a log is an
// ordinary accident — a `dbg!`, a `tracing::debug!(?source)` — and an accident
// must not disclose a secret. The other store crates follow the same rule.
impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource")
            .field("url", &redacted(&self.url))
            .field("reference", &self.reference)
            .field("keys", &self.keys)
            .field("format", &self.format)
            .field("credential", &self.credential)
            .field("tls", &self.tls)
            .field("working", &self.working)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Collects what a [`GitSource`] needs, and refuses what it cannot use.
///
/// Everything that can be wrong about a source — a path that escapes the
/// repository, a format nothing can infer, a commit id that is not one, a
/// working directory another source already holds — is decided here, at
/// [`build`](Self::build), rather than at the first fetch. A configuration
/// mistake should fail where it was made.
#[must_use]
pub struct Builder {
    url: String,
    reference: Reference,
    path: Option<Keys>,
    format: Option<Format>,
    credential: Credential,
    tls: TlsConfig,
    cache_dir: Option<PathBuf>,
    timeout: Duration,
    max_bytes: u64,
    compact_after: u32,
}

// Hand-written for the same reason [`GitSource`]'s is, and it is not
// redundant with it: a builder holds the URL from the moment it is created
// until `build` consumes it, so a `dbg!` or a `tracing::debug!(?builder)`
// during construction — the place a configuration is being got right, which
// is where people print things — would disclose exactly the credential the
// source is careful never to print.
impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("url", &redacted(&self.url))
            .field("reference", &self.reference)
            .field("path", &self.path)
            .field("format", &self.format)
            .field("credential", &self.credential)
            .field("tls", &self.tls)
            .field("cache_dir", &self.cache_dir)
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .field("compact_after", &self.compact_after)
            .finish()
    }
}

impl Builder {
    /// Reads from a branch, by short name. `main` unless this is called.
    pub fn branch(mut self, name: impl Into<String>) -> Self {
        self.reference = Reference::Branch(name.into());
        self
    }

    /// Reads from a tag, by short name.
    pub fn tag(mut self, name: impl Into<String>) -> Self {
        self.reference = Reference::Tag(name.into());
        self
    }

    /// Reads from one commit, by full hexadecimal object id.
    ///
    /// Reproducible, and static: nothing will ever move, so a watch on a
    /// pinned commit will never fire.
    pub fn commit(mut self, sha: impl Into<String>) -> Self {
        self.reference = Reference::Commit(sha.into());
        self
    }

    /// Reads from a [`Reference`] built elsewhere.
    pub fn reference(mut self, reference: Reference) -> Self {
        self.reference = reference;
        self
    }

    /// What to read: a file, or a [`Keys`] for several of them.
    ///
    /// A path is `/`-separated and relative to the repository root. Required.
    ///
    /// ```no_run
    /// # use dynamic_config_git::{GitSource, Keys};
    /// # fn example() -> Result<(), dynamic_config::Error> {
    /// // One file — what a bare string has always meant.
    /// GitSource::builder("https://github.com/acme/config.git")
    ///     .path("services/api/config.yaml")
    ///     .build()?;
    ///
    /// // Several, merged in this order: `local` wins where they overlap.
    /// GitSource::builder("https://github.com/acme/config.git")
    ///     .path(Keys::several([
    ///         "services/api/base.yaml",
    ///         "services/api/local.yaml",
    ///     ]))
    ///     .build()?;
    ///
    /// // A directory of disjoint sections, where an overlap is a mistake.
    /// GitSource::builder("https://github.com/acme/config.git")
    ///     .path(Keys::prefix("services/api"))
    ///     .format(dynamic_config::Format::Yaml)
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn path(mut self, path: impl Into<Keys>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// The format to parse the files as.
    ///
    /// Inferred from the extension when this is not called, so `config.yaml`
    /// needs nothing. Call it for a file whose name does not say — `.config`,
    /// or no extension at all — for a [`Keys::Several`] whose members name two
    /// different formats, and always for [`Keys::Prefix`], because a directory
    /// has no extension to read.
    ///
    /// One source reads one format. A caller who wants a JSON file and a TOML
    /// file has two sources, which already works.
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// How to authenticate. Anonymous — a public repository — by default.
    ///
    /// See [`Credential`]; the short version is that anything that expires
    /// should come from [`Credential::expiring`] rather than be pasted in as a
    /// string.
    pub fn credential(mut self, credential: Credential) -> Self {
        self.credential = credential;
        self
    }

    /// How to trust an `https://` host this machine does not already trust,
    /// and how to prove who is asking.
    ///
    /// For an enterprise GitLab behind a private certificate authority, and for
    /// a host that wants a client certificate before it will say hello. The
    /// platform's own trust store still applies, so one source configuration
    /// reaches both a private host and github.com.
    ///
    /// ```no_run
    /// # use dynamic_config_git::{GitSource, TlsConfig};
    /// # fn example() -> Result<(), dynamic_config::Error> {
    /// GitSource::builder("https://gitlab.internal/acme/config.git")
    ///     .path("services/api/config.yaml")
    ///     .tls(
    ///         TlsConfig::new()
    ///             .with_ca_certificate_file("/etc/ssl/certs/acme-root.pem")
    ///             .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
    ///     )
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// **This is the `https://` knob and only that one.** An `ssh://` remote
    /// authenticates its host through `known_hosts` and its client through a
    /// key, which is [`Credential::ssh_agent`], [`Credential::ssh_key`] or
    /// [`Credential::ssh_command`]; asking for both is refused at
    /// [`build`](Self::build) rather than half-applied.
    ///
    /// There is no way to turn verification off, and [`tls`](mod@tls) argues
    /// why at length — the short version being that a fetch presents its
    /// credential before it has received anything, so an unverified connection
    /// is one that hands a token to whoever is on the path.
    ///
    /// Configuring this replaces `gix`'s HTTP transport with one this crate
    /// builds, because `gix`'s ignores every TLS option it is given; see
    /// [`tls`](mod@tls) for what was measured. Leaving it alone changes
    /// nothing.
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Keeps the object database here, instead of in a temporary directory.
    ///
    /// It survives restarts, so a restarted process transfers almost nothing.
    /// In exchange the caller owns its size; see [`working`](mod@working).
    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(path.into());
        self
    }

    /// How long one fetch may take before it is given up on. Thirty seconds by
    /// default.
    ///
    /// The deadline for **one fetch attempt**, excluding retries the underlying
    /// client performs — the same sentence every store in this family answers
    /// to. What it reaches depends on which transport the source uses, and this
    /// crate owes the reader the table rather than the sentence:
    ///
    /// | Phase | With [`tls`](Self::tls) — this crate's transport | Without — `gix`'s own |
    /// |---|---|---|
    /// | connecting | this number | twenty seconds, `gix`'s, not configurable |
    /// | the handshake and the ref advertisement | this number, per read | **unbounded** |
    /// | negotiation and the pack | this number, per read | this number |
    ///
    /// `gix` takes an interrupt flag and checks it between packets, which is a
    /// real deadline for the part that transfers data and no deadline at all
    /// for a host that accepts the connection and then sends nothing — there
    /// are no packets for the check to be between. Its `reqwest` transport
    /// reads none of the timeouts its own options type carries, so that column
    /// is not this crate's to fix from outside; [`tls`](mod@tls) records what
    /// was measured and what closing it would have cost everybody else.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The largest **single file** this source will read, in bytes. A megabyte
    /// by default.
    ///
    /// Checked against the object header before any of the file is loaded, so
    /// a repository offering a two-gigabyte blob costs an error rather than the
    /// memory. Per file rather than per document: a [`Keys::Prefix`] read is
    /// bounded by this and by the five-hundred-and-twelve-file budget together.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// How many transfers a working directory may accumulate before it is
    /// emptied and refilled by the next fetch. Thirty-two by default; `0`
    /// turns it off.
    ///
    /// A shallow fetch of a moving branch adds a pack every time the branch
    /// moves and removes nothing, so a watcher left running grows without
    /// bound. Compaction is how that is answered, and it is a *visible*
    /// trigger on purpose: a store that deletes things should be a store the
    /// caller can see deleting them, and can stop.
    ///
    /// Only ever a directory this crate created. A [`cache_dir`](Self::cache_dir)
    /// pointing at a repository that already existed is never touched,
    /// whatever this is set to — see [`working`](mod@working) for the rule.
    ///
    /// Turn it off for a deployment that would rather run `git gc
    /// --prune=now` on its own cadence.
    pub fn compact_after(mut self, transfers: u32) -> Self {
        self.compact_after = transfers;
        self
    }

    /// Builds the source.
    ///
    /// # Errors
    ///
    /// If no path was given, or a [`Keys::Several`] with nothing in it; if any
    /// path is not a path inside the repository — absolute, or with a `.` or
    /// `..` component, or with an empty one; if the format was neither given
    /// nor inferable, or if two paths name two different formats; if a commit
    /// was named that is not a hexadecimal object id; or if the named working
    /// directory already belongs to another source in this program.
    pub fn build(self) -> Result<GitSource, Error> {
        let keys = self
            .path
            .ok_or_else(|| Error::remote("git: no path; call `path` with the file to read"))?;

        check_keys(&keys)?;
        check_reference(&self.reference)?;
        tls::check_scheme(&self.url, &self.tls)?;

        let format = match self.format {
            Some(format) => format,
            None => infer_format(&keys)?,
        };

        let working = match self.cache_dir {
            Some(directory) => Working::Named(working::Claimed::new(directory)?),
            None => Working::Temporary(working::Temporary::new()?),
        };

        Ok(GitSource {
            url: self.url,
            reference: self.reference,
            keys,
            format,
            credential: Session::new(self.credential),
            tls: self.tls,
            working,
            timeout: self.timeout,
            max_bytes: self.max_bytes,
            compact_after: self.compact_after,
            last: Mutex::new(None),
            fetching: Mutex::new(()),
        })
    }
}

/// Refuses anything in `keys` that is not a place inside the repository.
///
/// # Errors
///
/// If a named list is empty, or any path fails [`check_path`].
fn check_keys(keys: &Keys) -> Result<(), Error> {
    if let Keys::Several(paths) = keys {
        if paths.is_empty() {
            return Err(Error::remote(
                "git: `Keys::several` with no paths in it; name at least one \
                 file, or use `Keys::prefix` for a directory",
            ));
        }
    }

    if let Keys::Prefix(directory) = keys {
        // The repository root, which is what an empty directory name means,
        // needs no checking and has no components to check.
        let root = directory.trim_end_matches('/');

        return if root.is_empty() {
            Ok(())
        } else {
            check_path(root)
        };
    }

    keys.named().iter().try_for_each(|path| check_path(path))
}

/// The format the paths agree on, or an error naming the call that settles it.
///
/// [`documents::agreed_format`] is the family's rule and the family's wording:
/// two paths naming two formats is a mistake worth catching by name, because
/// parsing `server.toml` as JSON produces a syntax error about a file that has
/// no syntax error in it.
fn infer_format(keys: &Keys) -> Result<Format, Error> {
    match documents::agreed_format(keys.named()) {
        Err(complaint) => Err(Error::remote(format!("git: {complaint}"))),
        Ok(Some(format)) => Ok(format),
        Ok(None) => Err(Error::remote(format!(
            "git: cannot tell what format {} is; call `format`",
            keys.describe()
        ))),
    }
}

/// Refuses a path that is not a path inside the repository.
///
/// Nothing is ever written to the filesystem from the tree, so none of these
/// could escape a directory even if they were accepted — but a `..` in a
/// configured path means the caller believes something this crate does not do,
/// and a belief like that is worth failing on rather than silently reading
/// nothing.
///
/// It is also run over every path a [`Keys::Prefix`] walk *discovers*, where
/// the belief in question is the remote repository's: a tree entry's name is
/// bytes the host chose, and `git mktree` will write one called `..` without
/// complaint.
pub(crate) fn check_path(path: &str) -> Result<(), Error> {
    let refuse = |why: &str| {
        Err(Error::remote(format!(
            "git: {path:?} is not a file inside the repository: {why}"
        )))
    };

    if path.is_empty() {
        return refuse("it is empty");
    }

    if path.starts_with('/') {
        return refuse("it is absolute; paths are relative to the repository root");
    }

    for component in path.split('/') {
        match component {
            "" => return refuse("it has an empty component"),
            "." | ".." => {
                return refuse(
                    "it has a `.` or `..` component; this source reads one blob out of \
                     one tree and cannot leave the repository",
                )
            }
            _ => {}
        }
    }

    Ok(())
}

/// Refuses a commit id that is not one.
fn check_reference(reference: &Reference) -> Result<(), Error> {
    let Reference::Commit(sha) = reference else {
        return Ok(());
    };

    // 40 for SHA-1, 64 for SHA-256. An abbreviation is refused rather than
    // resolved: the protocol cannot ask for one, so accepting it here would
    // only move the failure to the first fetch.
    let usable = matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit());

    if usable {
        Ok(())
    } else {
        Err(Error::remote(format!(
            "git: {sha:?} is not a full commit id; give all {} characters, or \
             name a branch or a tag",
            if sha.len() > 40 { 64 } else { 40 }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_tries_to_leave_the_repository_is_refused() {
        for path in [
            "../../etc/shadow",
            "services/../../../etc/shadow",
            "/etc/shadow",
            "services//config.yaml",
            "./config.yaml",
            "",
        ] {
            let error = GitSource::builder("https://github.com/acme/config.git")
                .path(path)
                .format(Format::Yaml)
                .build()
                .expect_err("{path} must not be accepted");

            assert!(
                error
                    .to_string()
                    .contains("not a file inside the repository")
                    || error.to_string().contains("no path"),
                "{path}: {error}"
            );
        }
    }

    #[test]
    fn an_ordinary_path_is_accepted_and_its_format_inferred() {
        let source = GitSource::builder("https://github.com/acme/config.git")
            .path("services/api/config.yaml")
            .build()
            .expect("a yaml file needs no format");

        assert_eq!(source.format, Format::Yaml);
        assert_eq!(source.reference, Reference::Branch("main".to_owned()));
    }

    #[test]
    fn a_file_whose_name_says_nothing_needs_a_format() {
        let error = GitSource::builder("https://github.com/acme/config.git")
            .path("services/api/settings")
            .build()
            .expect_err("nothing can infer a format from that");

        assert!(
            error.to_string().contains("cannot tell what format"),
            "{error}"
        );

        GitSource::builder("https://github.com/acme/config.git")
            .path("services/api/settings")
            .format(Format::Toml)
            .build()
            .expect("saying so is all it takes");
    }

    #[test]
    fn an_abbreviated_commit_is_refused_where_it_was_written() {
        let error = GitSource::builder("https://github.com/acme/config.git")
            .path("config.yaml")
            .commit("deadbee")
            .build()
            .expect_err("the protocol cannot ask for an abbreviation");

        assert!(
            error.to_string().contains("not a full commit id"),
            "{error}"
        );

        GitSource::builder("https://github.com/acme/config.git")
            .path("config.yaml")
            .commit("da39a3ee5e6b4b0d3255bfef95601890afd80709")
            .build()
            .expect("a full sha is fine");
    }

    #[test]
    fn each_reference_asks_for_the_ref_it_names() {
        assert_eq!(
            Reference::Branch("main".to_owned()).refspec(),
            "+refs/heads/main:refs/dynamic-config/head"
        );
        assert_eq!(
            Reference::Tag("v1.2.0".to_owned()).refspec(),
            "+refs/tags/v1.2.0:refs/dynamic-config/head"
        );
        assert_eq!(
            Reference::Commit("da39a3ee5e6b4b0d3255bfef95601890afd80709".to_owned()).refspec(),
            "+da39a3ee5e6b4b0d3255bfef95601890afd80709:refs/dynamic-config/head"
        );
    }

    /// The planted-credential test every store in this family carries: a token
    /// in the URL must not reach `Debug` or `describe()`, which are the two
    /// strings that end up in logs.
    #[test]
    fn a_token_in_the_url_reaches_neither_debug_nor_describe() {
        let source =
            GitSource::builder("https://x-access-token:ghs_hunter2@github.com/acme/config.git")
                .path("config.yaml")
                .credential(Credential::token("ghs_hunter2-as-well"))
                .build()
                .unwrap();

        let printed = format!("{source:?} {}", source.describe());

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("github.com/acme/config.git"), "{printed}");
        // The user half survives, which is what makes the redaction usable
        // rather than a black hole.
        assert!(printed.contains("x-access-token"), "{printed}");
    }

    /// The same planted credential, one step earlier. A builder holds the
    /// URL from `builder()` to `build()`, and construction is exactly where
    /// somebody prints things to see what they have configured.
    #[test]
    fn a_token_in_the_url_does_not_reach_the_builders_debug_either() {
        let builder =
            GitSource::builder("https://x-access-token:ghs_hunter2@github.com/acme/config.git")
                .path("config.yaml")
                .credential(Credential::token("ghs_hunter2-as-well"));

        let printed = format!("{builder:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("github.com/acme/config.git"), "{printed}");
    }
}
