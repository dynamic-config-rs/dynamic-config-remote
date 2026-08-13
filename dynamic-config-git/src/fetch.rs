//! One fetch: what is asked of the host, and what is read back out.
//!
//! Everything that touches `gix` is here, so the rest of the crate reads as
//! configuration handling rather than git plumbing.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dynamic_config::Error;
use dynamic_config_store_core::documents;
use gix::remote::Direction;

use crate::auth::Auth;
use crate::url::redacted;
use crate::{check_path, Keys, Reference};

/// A failed git operation, sorted by whether a different credential would fix
/// it.
///
/// The distinction is the only thing a reload loop can act on: `Refused` will
/// not fix itself by waiting, and every other failure might.
pub(crate) enum Failure {
    /// **The host refused what was presented**, so obtaining a different
    /// credential and trying once more is worth doing. Always carries an
    /// [`ErrorKind::Auth`](dynamic_config::ErrorKind::Auth) error.
    ///
    /// A credential that could not be *obtained* is not this: there is nothing
    /// to replace, and asking the same closure again in the same breath would
    /// only fail the same way.
    Refused(Error),
    /// Everything else — an unreachable host, a ref that does not exist, a
    /// document that is not a document, a closure that could not produce a
    /// credential. Carries the error classified where it was produced, which
    /// is `Remote` for everything this module raises itself.
    Other(Error),
}

impl Failure {
    pub(crate) fn into_error(self) -> Error {
        match self {
            Self::Refused(error) | Self::Other(error) => error,
        }
    }
}

/// Opens the object database in `dir`, creating it on the first fetch.
///
/// Bare: there is no working tree, nothing is ever checked out, and no file
/// this crate did not create is written into `dir`. That is not an
/// optimisation, it is the traversal defence — a repository whose tree
/// contains `../../etc/shadow` as a name, or a symlink pointing there, cannot
/// make a checkout that never happens write anywhere.
///
/// `ssh_command`, when the credential names one, is applied as an in-memory
/// `core.sshCommand` override marked as coming from the command line. In
/// memory, because a value written to `dir`'s config would outlive the fetch,
/// and a caller's `SshAuth::Command` may carry anything.
///
/// A database this call **creates** is stamped with
/// [`working::MARKER`](crate::working::MARKER), which is what later licenses
/// [`working::compact`](crate::working::compact) to empty it. One that was
/// already there is opened and never stamped, so a caller who named a
/// repository that exists keeps it.
pub(crate) fn open(dir: &Path, ssh_command: Option<String>) -> Result<gix::Repository, Failure> {
    let overrides: Vec<String> = ssh_command
        .map(|command| format!("core.sshCommand={command}"))
        .into_iter()
        .collect();

    let options = gix::open::Options::default().cli_overrides(overrides.iter().map(String::as_str));

    match gix::open_opts(dir, options.clone()) {
        Ok(repository) => Ok(repository),
        Err(_) => {
            // Not a repository yet — the first fetch of a temporary directory,
            // or of a caller-named one that has never been used.
            gix::init_bare(dir).map_err(|error| {
                Failure::Other(Error::remote(format!(
                    "git: cannot prepare the working directory {}: {error}",
                    dir.display()
                )))
            })?;

            crate::working::mark(dir).map_err(Failure::Other)?;

            gix::open_opts(dir, options).map_err(|error| {
                Failure::Other(Error::remote(format!(
                    "git: cannot open the working directory {}: {error}",
                    dir.display()
                )))
            })
        }
    }
}

/// Everything one fetch needs that is not the repository itself.
///
/// A struct rather than eight parameters, because the two transports below take
/// exactly the same set and a positional list of five borrowed things is a
/// place mistakes hide.
pub(crate) struct Plan<'a> {
    pub(crate) url: &'a str,
    pub(crate) reference: &'a Reference,
    pub(crate) auth: &'a Auth,
    pub(crate) tls: &'a dynamic_config_store_core::tls::TlsConfig,
    pub(crate) timeout: Duration,
    /// The source's `describe()`, for the errors the TLS half raises itself.
    pub(crate) described: &'a str,
}

/// Asks the host where `reference` points, and receives the objects if they
/// are not here already.
///
/// One connection does both. The handshake and the ref advertisement are what
/// `git ls-remote` costs — a few hundred bytes and no objects — and the pack
/// is only asked for when the commit turns out to be one this repository does
/// not have. An unchanged ref therefore transfers nothing, which is what makes
/// polling a git host at configuration cadence reasonable.
///
/// `want_objects` is `false` for the watch loop's idle check, which only needs
/// to know whether the ref moved.
#[allow(
    clippy::result_large_err,
    reason = "the error is `gix_credentials::protocol::Error`, which is not \
              ours to box; the credential closure's signature is `gix`'s"
)]
pub(crate) fn fetch(
    repository: &gix::Repository,
    plan: &Plan<'_>,
    want_objects: bool,
) -> Result<gix::ObjectId, Failure> {
    let Plan {
        url,
        reference,
        tls,
        timeout,
        described,
        ..
    } = plan;
    let (url, timeout) = (*url, *timeout);

    let remote = repository
        .remote_at(url)
        .map_err(|error| other(url, format_args!("the url is not usable: {error}")))?
        // Tags are refs we did not ask for. Fetching them would turn a
        // one-commit transfer into every release this repository ever cut.
        .with_fetch_tags(gix::remote::fetch::Tags::None)
        .with_refspecs([reference.refspec().as_str()], Direction::Fetch)
        .map_err(|error| {
            other(
                url,
                format_args!("{} is not a valid ref: {error}", reference),
            )
        })?;

    // Two transports, and the split is the whole of what TLS costs. Without a
    // `TlsConfig` this is `gix`'s own, unchanged — so no source that existed
    // before this feature takes a different code path. With one it is
    // `crate::tls`'s, because `gix`'s reads no TLS option it is given.
    if tls.is_empty() {
        let connection = remote
            .connect(Direction::Fetch)
            .map_err(|error| classify(url, &error, format_args!("cannot connect")))?;

        return negotiate(repository, connection, plan, want_objects);
    }

    let (sanitized, version) = remote
        .sanitized_url_and_version(Direction::Fetch)
        .map_err(|error| classify(url, &error, format_args!("cannot connect")))?;

    let transport = crate::tls::transport(
        &sanitized,
        version,
        tls,
        timeout,
        repository
            .config_snapshot()
            .boolean("gitoxide.trace.packet")
            == Some(true),
        described,
    )
    .map_err(Failure::Other)?;

    negotiate(
        repository,
        remote.to_connection_with_transport(transport),
        plan,
        want_objects,
    )
}

/// The handshake, the ref advertisement and — if the commit is new — the pack.
///
/// Generic over the transport so the two ways of obtaining one share every
/// decision after it: the credential helper, the resolution by name, the
/// "already have it" short circuit and the deadline.
#[allow(
    clippy::result_large_err,
    reason = "the error is `gix_credentials::protocol::Error`, which is not \
              ours to box; the credential closure's signature is `gix`'s"
)]
fn negotiate<T>(
    repository: &gix::Repository,
    connection: gix::remote::Connection<'_, '_, '_, T>,
    plan: &Plan<'_>,
    want_objects: bool,
) -> Result<gix::ObjectId, Failure>
where
    T: gix::protocol::transport::client::blocking_io::Transport,
{
    let Plan {
        url,
        reference,
        auth,
        timeout,
        ..
    } = plan;
    let (url, timeout) = (*url, *timeout);

    let identity = match auth {
        Auth::Https { username, password } => Some(gix::sec::identity::Account {
            username: username.clone(),
            password: password.clone(),
            oauth_refresh_token: None,
        }),
        // SSH carries its own identity in the `ssh` process, and an anonymous
        // fetch presents nothing. Both answer the helper with "no credential",
        // which is also what stops `git`'s configured credential helpers from
        // being consulted behind the caller's back.
        Auth::Anonymous | Auth::Ssh(_) => None,
    };

    let connection = connection.with_credentials(move |action| match action {
        gix::credentials::helper::Action::Get(context) => {
            Ok(identity
                .clone()
                .map(|identity| gix::credentials::protocol::Outcome {
                    identity,
                    next: context.into(),
                }))
        }
        // `Store` and `Erase` are the helper protocol asking us to remember or
        // forget. There is nothing to remember: the credential came from the
        // caller's closure and will be asked for again.
        gix::credentials::helper::Action::Store(_) | gix::credentials::helper::Action::Erase(_) => {
            Ok(None)
        }
    });

    let deadline = Deadline::start(timeout);

    let prepared = connection
        .prepare_fetch(
            gix::progress::Discard,
            gix::remote::ref_map::Options::default(),
        )
        .map_err(|error| classify(url, &error, format_args!("cannot list {reference}")))?;

    let commit = reference.resolve(prepared.ref_map(), url)?;

    // The commit is already here: the ref did not move since the last fetch,
    // and a pack we would only throw away is never asked for. A fetch
    // delivers a pack that is complete for the commit it was asked for, so
    // having the commit means having its tree and every blob in it.
    if !want_objects || repository.has_object(commit) {
        return Ok(commit);
    }

    prepared
        .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(DEPTH))
        .receive(gix::progress::Discard, deadline.interrupt())
        .map_err(|error| {
            if deadline.expired() {
                return Failure::Other(Error::remote(format!(
                    "git {}: fetching {reference} took longer than {timeout:?}",
                    redacted(url)
                )));
            }

            classify(url, &error, format_args!("cannot fetch {reference}"))
        })?;

    Ok(commit)
}

/// One commit, no history.
///
/// A configuration reader needs the file at a commit, never the commits before
/// it. `git log` on the working directory will disappoint; that is the trade,
/// and it is why the working directory is a cache rather than a clone somebody
/// might want to use.
const DEPTH: NonZeroU32 = match NonZeroU32::new(1) {
    Some(depth) => depth,
    None => unreachable!(),
};

/// Reads everything a source asks for **out of one commit's tree**.
///
/// This is the whole reason a git store can do what the key-value stores in
/// this family cannot. A fetch resolves one commit; that commit has one tree;
/// every path below is read out of *that* tree. So a set of files is read as of
/// one instant with no transaction, no listing race and no second round trip —
/// the atomicity is a property of the object model rather than something this
/// crate arranges. See [`Keys`](crate::Keys).
///
/// The pairs come back in merge order: the order the caller wrote for
/// [`Keys::Several`](crate::Keys::Several), and tree order — which git keeps
/// sorted — for [`Keys::Prefix`](crate::Keys::Prefix), whose members must not
/// overlap anyway.
///
/// `max_bytes` bounds **one file and the whole read** — see [`Budget`].
pub(crate) fn read_documents(
    repository: &gix::Repository,
    commit: gix::ObjectId,
    keys: &Keys,
    max_bytes: u64,
    url: &str,
) -> Result<Vec<(String, String)>, Failure> {
    let at = |path: &str, what: std::fmt::Arguments<'_>| {
        Failure::Other(Error::remote(format!(
            "git {} {}:{path}: {what}",
            redacted(url),
            commit.to_hex_with_len(12),
        )))
    };

    // Read once, before any path is looked up: every document below therefore
    // comes from the same tree, which is what makes the set atomic.
    let tree = repository
        .find_object(commit)
        .map_err(|error| at("", format_args!("the commit is not readable: {error}")))?
        .peel_to_tree()
        .map_err(|error| at("", format_args!("the commit has no tree: {error}")))?;

    let paths = match keys {
        Keys::One(path) => vec![path.clone()],
        Keys::Several(paths) => paths.clone(),
        Keys::Prefix(prefix) => {
            let described = format!("git {} {}", redacted(url), commit.to_hex_with_len(12));

            under(&tree, prefix, &described).map_err(Failure::Other)?
        }
    };

    // A named list is the caller's own and cannot be empty — `build` refuses
    // that — so this only ever fires for a directory with nothing in it, which
    // is a missing configuration rather than an empty one.
    if paths.is_empty() {
        return Err(at(
            &keys.describe(),
            format_args!("nothing is there at this commit, so there is nothing to load"),
        ));
    }

    let mut documents = Vec::with_capacity(paths.len());
    let mut budget = Budget::of(max_bytes);

    for path in paths {
        // Every path must answer. Merging the four that did would leave a
        // process running a configuration with a section quietly missing from
        // it, which is worse than a refresh that failed and left the last
        // known good document serving.
        let text = read_blob(repository, &tree, &path, &mut budget, &|what| {
            at(&path, what)
        })?;

        documents.push((path, text));
    }

    Ok(documents)
}

/// How many bytes one read may take, in total and in any one file.
///
/// **One number, not a product.** `max_bytes` used to bound a single file only,
/// so a directory read was bounded at the file budget times the key budget —
/// five hundred and twelve megabytes at the defaults, which is a number nobody
/// chose and nobody would have. A caller who says a megabyte is saying what
/// this source may load, and there is no reading of that sentence under which
/// naming a directory instead of a file multiplies it by five hundred.
///
/// The count budget stays as well, because the two bound different things: a
/// tree of a hundred thousand empty files costs the *walk* rather than the
/// memory, and [`documents::MOST_KEYS`] is what stops that.
struct Budget {
    /// The whole of it, kept for the message: the number the caller wrote is
    /// the number an error should quote back.
    max_bytes: u64,
    /// What is left after the files already read.
    left: u64,
}

impl Budget {
    fn of(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            left: max_bytes,
        }
    }
}

/// Reads one blob out of an already-resolved tree.
///
/// `path` is a git path — `/`-separated, relative to the repository root — and
/// is looked up entry by entry in the object database. Nothing here touches
/// the filesystem, so no name in the tree can name a place outside the working
/// directory. What the tree *can* do is offer something that is not a
/// document, and each of those is an error rather than a surprise:
///
/// - a **symlink**, which git stores as a blob whose content is a path. Reading
///   it as configuration would silently make the document be whatever that
///   path resolved to — on a checkout, a file outside the repository entirely.
/// - a **directory**, which is not one document.
/// - a **submodule**, which is a commit in a repository this fetch never saw.
/// - a blob **larger than the [`Budget`]**, refused from the object header
///   before a byte of it is loaded, because the remote decides how big it is —
///   and so is one that fits on its own but takes the read past what is left.
/// - a blob that is **not UTF-8**, which no configuration format is.
fn read_blob(
    repository: &gix::Repository,
    tree: &gix::Tree<'_>,
    path: &str,
    budget: &mut Budget,
    at: &dyn Fn(std::fmt::Arguments<'_>) -> Failure,
) -> Result<String, Failure> {
    // Split on `/` rather than handing the string to a `Path`: a git path is
    // always `/`-separated, and letting the host platform decide would make
    // `a\b` two components on Windows and one everywhere else.
    let entry = tree
        .lookup_entry(path.split('/').map(str::as_bytes))
        .map_err(|error| at(format_args!("the tree is not readable: {error}")))?
        .ok_or_else(|| at(format_args!("there is no such file at this commit")))?;

    match entry.mode().kind() {
        gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => {}
        gix::object::tree::EntryKind::Tree => {
            return Err(at(format_args!(
                "that is a directory; a source reads a document, and a whole \
                 directory of them is `Keys::prefix`"
            )))
        }
        gix::object::tree::EntryKind::Link => {
            return Err(at(format_args!(
                "that is a symbolic link; this crate will not follow one, \
                 because where it points is the remote repository's choice \
                 and not this program's"
            )))
        }
        gix::object::tree::EntryKind::Commit => {
            return Err(at(format_args!(
                "that is a submodule; its objects are in another repository"
            )))
        }
    }

    // The header, not the object: the size is the remote's to choose, and a
    // two-gigabyte blob must be refused rather than allocated.
    let size = repository
        .find_header(entry.object_id())
        .map_err(|error| at(format_args!("the blob is not readable: {error}")))?
        .size();

    // Two limits, answering different questions. The per-file one asks *is
    // this a configuration file at all*; the budget asks *what may one read
    // cost*, which for a directory is the number a caller can reason about —
    // the bound it replaces was `max_bytes` times the key budget, a product
    // nobody chose.
    let max_bytes = budget.max_bytes;

    if size > max_bytes {
        return Err(at(format_args!(
            "the file is {size} bytes, over the {max_bytes}-byte limit; \
             raise it with `max_bytes` if that is really a configuration file"
        )));
    }

    if size > budget.left {
        return Err(at(format_args!(
            "the files read so far and this one come to more than the \
             {max_bytes}-byte limit, which is what one read may cost in \
             total; name fewer files, or raise `max_bytes`"
        )));
    }

    budget.left -= size;

    let object = repository
        .find_object(entry.object_id())
        .map_err(|error| at(format_args!("the blob is not readable: {error}")))?;

    String::from_utf8(object.data.clone())
        .map_err(|_| at(format_args!("the file is not valid UTF-8")))
}

/// Every file below `directory`, in tree order, as repository-root paths.
///
/// Recursive, because a configuration directory holding `db/` and `server/` is
/// an ordinary layout. The listing is where a hostile repository gets its one
/// chance at this crate, so it is bounded and checked rather than trusted:
///
/// - **the key budget** ([`MOST_KEYS`](documents::MOST_KEYS)) stops at the
///   first file past it, so a prefix pointed at a large repository costs a walk
///   of five hundred entries rather than a process's memory;
/// - **the same cap on directories descended into**, because the budget on
///   files cannot see a tree of ten thousand empty directories;
/// - **every discovered path is re-checked** with [`check_path`] and against
///   the prefix itself. A tree entry's name is bytes the remote chose, and
///   `git mktree` will happily write one called `..`; `git` refuses such a
///   tree, this crate does not have to find out whether every host does.
///
/// A symlink or a submodule *below* a prefix is left to
/// [`read_blob`] to refuse by name rather than skipped here. Skipping would let
/// a repository make a section of a configuration disappear by replacing its
/// file with a link — silently, which is the failure this crate exists to
/// avoid.
fn under(tree: &gix::Tree<'_>, directory: &str, described: &str) -> Result<Vec<String>, Error> {
    let root = directory.trim_end_matches('/');

    let mut start = tree.clone();

    if !root.is_empty() {
        let entry = start
            .lookup_entry(root.split('/').map(str::as_bytes))
            .map_err(|error| {
                Error::remote(format!(
                    "{described}: {root}: the tree is not readable: {error}"
                ))
            })?
            .ok_or_else(|| {
                Error::remote(format!(
                    "{described}: {root}: there is no such directory at this commit"
                ))
            })?;

        if entry.mode().kind() != gix::object::tree::EntryKind::Tree {
            return Err(Error::remote(format!(
                "{described}: {root}: that is a file rather than a directory; \
                 name it as one path instead of as a prefix"
            )));
        }

        start = entry
            .object()
            .map_err(|error| {
                Error::remote(format!("{described}: {root}: it is not readable: {error}"))
            })?
            .peel_to_tree()
            .map_err(|error| {
                Error::remote(format!(
                    "{described}: {root}: it is not a directory: {error}"
                ))
            })?;
    }

    let mut found: Vec<String> = Vec::new();
    let mut pending = vec![(root.to_owned(), start)];
    let mut descended = 0_usize;

    while let Some((at, tree)) = pending.pop() {
        for entry in tree.iter() {
            let entry = entry.map_err(|error| {
                Error::remote(format!(
                    "{described}: {at}: the tree is not readable: {error}"
                ))
            })?;

            // Not `to_string()`: `BStr`'s `Display` is lossy, and a name that
            // came back as `services/api/�.yaml` would be a path this crate
            // then went looking for and could not find. A tree entry whose
            // name is not UTF-8 is refused as itself.
            let name = std::str::from_utf8(entry.filename()).map_err(|_| {
                Error::remote(format!(
                    "{described}: {at}: a file here has a name that is not valid UTF-8"
                ))
            })?;

            let path = if at.is_empty() {
                name.to_owned()
            } else {
                format!("{at}/{name}")
            };

            // The name came from the remote. Both checks are one comparison
            // each and they are what stops a tree entry called `..` from
            // turning a prefix read into a read of something else.
            check_path(&path).map_err(|error| Error::remote(format!("{described}: {error}")))?;

            if !root.is_empty() {
                documents::under_prefix(&path, &format!("{root}/"), described)?;
            }

            if entry.mode().kind() == gix::object::tree::EntryKind::Tree {
                descended += 1;
                documents::within_key_budget(descended, described)?;

                pending.push((
                    path,
                    entry
                        .object()
                        .map_err(|error| {
                            Error::remote(format!("{described}: the tree is not readable: {error}"))
                        })?
                        .peel_to_tree()
                        .map_err(|error| {
                            Error::remote(format!("{described}: the tree is not readable: {error}"))
                        })?,
                ));

                continue;
            }

            found.push(path);

            // Per entry, so a prefix over a large repository is refused during
            // the walk rather than after it.
            documents::within_key_budget(found.len(), described)?;
        }
    }

    // Tree order is per directory; the walk interleaves several. Sorting makes
    // one set of files produce one document and one diagnostic every time,
    // which a collision report has to be able to promise.
    found.sort();

    Ok(found)
}

/// A `Remote`-kinded failure naming the store.
fn other(url: &str, what: std::fmt::Arguments<'_>) -> Failure {
    Failure::Other(Error::remote(format!("git {}: {what}", redacted(url))))
}

/// Sorts a `gix` failure by whether a different credential would help.
///
/// `gix` reports a refused credential as an `io::Error` of kind
/// `PermissionDenied` somewhere down the source chain — the HTTP transport
/// turns a 401 into one, and `ssh` exiting on a rejected key produces the
/// same. Walking the chain is how the classification stays typed: matching on
/// the *rendered* message would call a repository named `401-service` a
/// credential problem.
///
/// The message is `gix`'s own rendering, which never contains the request. The
/// url is ours, and redacted before it goes anywhere.
fn classify(
    url: &str,
    error: &(dyn std::error::Error + 'static),
    what: std::fmt::Arguments<'_>,
) -> Failure {
    let described = format!("git {}: {what}: {}", redacted(url), chain(error));

    if refused(error) {
        Failure::Refused(Error::auth(described))
    } else {
        Failure::Other(Error::remote(described))
    }
}

/// An error and everything under it, as one line.
///
/// `gix`'s own `Display` is the top of the chain and nothing else, so a fetch
/// that failed inside the transport renders as "An IO error occurred when
/// talking to the server" — true, and no use to anybody. The reason a
/// certificate was refused, or that a host answered 500, is two or three
/// `source()` calls down, and it is the whole diagnostic value of a TLS
/// feature.
///
/// Repeats are dropped rather than concatenated: several `gix` layers wrap the
/// same message, and "cannot connect: X: X: X" reads as a bug in this crate.
pub(crate) fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered: Vec<String> = Vec::new();
    let mut current = Some(error);

    while let Some(error) = current {
        let text = error.to_string();

        if !rendered.contains(&text) {
            rendered.push(text);
        }

        current = error.source();
    }

    rendered.join(": ")
}

/// Whether anything in this error's chain is a permission refusal.
fn refused(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);

    while let Some(error) = current {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
        }

        // `gix` refuses to send credentials over plain `http://` itself; that
        // is a credential problem the caller has to fix, not a network one.
        if let Some(gix::protocol::transport::client::Error::AuthenticationRefused(_)) =
            error.downcast_ref::<gix::protocol::transport::client::Error>()
        {
            return true;
        }

        current = error.source();
    }

    false
}

/// A wall-clock limit on one fetch, expressed the only way `gix` accepts one.
///
/// `gix` takes an interrupt flag and checks it between packets while
/// negotiating and while receiving a pack, so a watchdog raising the flag is a
/// real deadline for the part that transfers data. It is **not** a deadline on
/// connecting, nor on a host that accepts the connection and then sends
/// nothing — there are no packets for the check to be between. Those two are
/// the transport's to bound, and only one of the two transports can:
/// [`crate::tls`]'s client takes this same number as its connect and stall
/// deadline, while `gix`'s reqwest transport hardcodes a twenty-second connect
/// timeout, exposes no other, and waits forever on a silent host. See
/// [`Builder::with_timeout`](crate::Builder::with_timeout) for the table a
/// caller needs.
struct Deadline {
    interrupt: Arc<AtomicBool>,
    /// Raised by the watchdog, so a failure caused by the deadline can say so
    /// rather than surfacing as an interrupted read.
    expired: Arc<AtomicBool>,
    finished: Arc<(Mutex<bool>, Condvar)>,
}

impl Deadline {
    fn start(after: Duration) -> Self {
        let deadline = Self {
            interrupt: Arc::new(AtomicBool::new(false)),
            expired: Arc::new(AtomicBool::new(false)),
            finished: Arc::new((Mutex::new(false), Condvar::new())),
        };

        let interrupt = Arc::clone(&deadline.interrupt);
        let expired = Arc::clone(&deadline.expired);
        let finished = Arc::clone(&deadline.finished);

        std::thread::spawn(move || {
            let (lock, condition) = &*finished;
            let done = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // A condvar rather than a sleep: the watchdog for a fetch that
            // finished in a millisecond must not linger for the whole timeout,
            // or a poll loop accumulates one parked thread per tick.
            let (done, timed_out) = condition
                .wait_timeout_while(done, after, |done| !*done)
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            if timed_out.timed_out() && !*done {
                expired.store(true, Ordering::SeqCst);
                interrupt.store(true, Ordering::SeqCst);
            }
        });

        deadline
    }

    fn interrupt(&self) -> &AtomicBool {
        &self.interrupt
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::SeqCst)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let (lock, condition) = &*self.finished;

        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condition.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_credential_is_recognised_through_the_error_chain() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);

        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("could not fetch")
            }
        }

        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let refusal = Wrapper(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Received HTTP status 401",
        ));
        let outage = Wrapper(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));

        assert!(matches!(
            classify("https://host/r.git", &refusal, format_args!("x")),
            Failure::Refused(_)
        ));
        assert!(matches!(
            classify("https://host/r.git", &outage, format_args!("x")),
            Failure::Other(_)
        ));
    }

    #[test]
    fn a_deadline_that_expires_says_so() {
        let deadline = Deadline::start(Duration::from_millis(20));

        assert!(!deadline.expired());

        std::thread::sleep(Duration::from_millis(200));

        assert!(deadline.expired(), "the watchdog must fire");
        assert!(deadline.interrupt().load(Ordering::SeqCst));
    }

    #[test]
    fn a_deadline_that_is_dropped_first_never_fires() {
        let deadline = Deadline::start(Duration::from_secs(3600));
        let interrupt = Arc::clone(&deadline.interrupt);

        drop(deadline);
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            !interrupt.load(Ordering::SeqCst),
            "a fetch that finished must not leave a watchdog parked for an hour"
        );
    }
}
