//! Where the objects live between fetches, and who may read them.
//!
//! A git store needs somewhere to put an object database. That directory holds
//! the contents of a private repository, so it is created **private at
//! creation** — `0700`, by the call that creates it, never by a `chmod`
//! afterwards. A `chmod` after the fact leaves a window in which the directory
//! is world-readable, and a window is all an attacker on a shared host needs.
//! It is the same rule the core crate's on-disk cache follows.
//!
//! # Two places it can be
//!
//! **A temporary directory, by default.** Made when the first fetch runs,
//! removed when the source is dropped. Nothing survives the process, which is
//! the right default for a container: no growth to manage, no stale objects, no
//! secret left on a volume after the pod dies. It costs one full fetch per
//! process start.
//!
//! **A directory the caller names**, with [`GitSource::cache_dir`]. It survives
//! restarts, so a restart transfers almost nothing — worth it for a large
//! repository or a fleet that restarts often.
//!
//! # What a store may delete, and where
//!
//! Either directory grows: a shallow fetch of a moving branch writes one pack
//! per transfer, the old ones stop being reachable the moment the local ref
//! moves, and nothing in git removes them until a `gc`. A long-lived watcher
//! accumulates one pack per push for as long as it runs, and a caller-named
//! directory carries that on across restarts.
//!
//! So this crate compacts, and the rule it compacts under is narrow enough to
//! state in one line: **it deletes only what it wrote, only in a directory it
//! created for itself, and only on a trigger the caller can see and turn off.**
//!
//! - *what it wrote* — the working directory is a bare object database holding
//!   packs from this crate's fetches and one ref this crate writes. There is
//!   nothing else in it to lose, and what is removed is re-obtained by the very
//!   fetch that removed it.
//! - *a directory it created* — a marker file is written when this crate
//!   initialises the object database, and a directory without it is **never**
//!   touched. A caller who points [`cache_dir`](crate::Builder::cache_dir) at a
//!   repository that already exists gets the old behaviour, unpruned, rather
//!   than losing a repository to a store that assumed the directory was its
//!   own.
//! - *a visible trigger* — [`Builder::compact_after`](crate::Builder::compact_after)
//!   is the number of transfers a directory may accumulate, thirty-two by
//!   default, and `0` turns it off entirely for a caller who would rather run
//!   `git gc --prune=now` on their own cadence.
//!
//! The compaction is not a `gc`: it empties the object database and lets the
//! next fetch refill it, which is one full transfer every `compact_after`
//! pushes and leaves the directory holding exactly the current commit again.
//! Deleting a pack *and repacking* would be less to transfer and much more to
//! get wrong — a store that rewrites an object database is a store that can
//! corrupt one, and this one cannot: the only thing it can lose is a copy of
//! something the remote still has.
//!
//! It happens **before** a fetch that is going to read, never after one and
//! never on a watch's idle tick, so nothing the current call depends on is
//! removed and nothing is emptied that is not about to be refilled.
//!
//! # One directory, one source
//!
//! Two sources fetching into one directory would interleave their ref updates
//! and their packs. A caller-named directory is therefore **claimed** by the
//! source that names it, and a second source in the same program naming the
//! same directory is refused at construction — before anything is written —
//! rather than corrupting it. The default, a temporary directory per source,
//! cannot collide at all.
//!
//! # Two processes are still not detected, and that is a decision
//!
//! The claim above is in this process only. Two *programs* pointed at one
//! directory are not detected, are not supported, and this crate will not grow
//! a lock file to change that. The reasoning is worth writing down, because
//! "add a lock file" is the obvious answer and it is the wrong one here:
//!
//! - **A stale-lock heuristic is wrong exactly where sharing happens.** The
//!   deployment that shares a working directory is not two programs on one
//!   host; it is one volume mounted into two containers. Pid liveness cannot
//!   see across a pid namespace — both containers have a live pid 1, so a lock
//!   left by a dead process reads as held and a lock held by a live one reads
//!   as held for a different reason. An age bound replaces that with a guess
//!   about how long a fetch may take, and two nodes writing one network volume
//!   do not agree on the clock the guess is measured against.
//! - **An advisory lock the kernel releases has no staleness — and no reach.**
//!   `flock` would be the correct mechanism, and it is unreliable on precisely
//!   the network filesystems that make the sharing possible in the first place.
//!   It also costs a dependency in a crate whose small graph is one of the
//!   things it claims, to convert an unsupported configuration into a start-up
//!   failure on filesystems that do not implement locking.
//!
//! What is done instead is to bound the damage. Concurrent *fetches* into one
//! object database are what git itself is built for: objects are written to a
//! temporary file and renamed, and a ref update takes a `.lock`. Compaction is
//! the part this crate added, and it is why the marker above is required: a
//! program that empties a directory another program is reading costs that
//! program **one failed fetch**, which is a failure this crate already promises
//! to survive — the previously fetched document stays installed and a watch
//! waits out the interval. That is asserted rather than claimed:
//! `a_working_directory_emptied_by_another_program_costs_a_fetch_rather_than_the_source`
//! empties one underneath a live source and fetches again. Give each program
//! its own directory anyway.
//!
//! [`GitSource::cache_dir`]: crate::Builder::cache_dir

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use dynamic_config::Error;

/// How many transfers a working directory may accumulate before it is emptied.
///
/// One transfer is one pack, so this is also the number of pack files the
/// object database is allowed to hold. Thirty-two is chosen from both ends: a
/// configuration repository that moves twice a month reaches it in a year and a
/// half and effectively never compacts, while a branch that moves every few
/// minutes reaches it in an afternoon — and pays one full transfer of the
/// current tree for every thirty-two pushes, which is a bounded fraction of
/// what it was going to transfer anyway.
pub(crate) const AFTER: u32 = 32;

/// The file that says this directory is this crate's to empty.
///
/// Written when this crate initialises the object database, and checked before
/// anything is ever deleted. A directory that does not have it was not created
/// here — a caller-named path that already held a repository is the case that
/// matters — and is never compacted.
pub(crate) const MARKER: &str = "dynamic-config-store";

/// What [`MARKER`] says to whoever finds it.
const MARKER_TEXT: &str = "\
This directory is the object cache of a `dynamic-config-git` source.

Everything in it was fetched by that crate, and that crate empties it once it
holds more packs than `GitSource::builder(..).compact_after(..)` allows —
re-fetching what it needs. Do not keep anything here.
";

/// Names the temporary directories apart within one process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The caller-named directories some live source is using.
static CLAIMED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

fn claimed() -> std::sync::MutexGuard<'static, Option<HashSet<PathBuf>>> {
    CLAIMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where a source keeps its object database.
#[derive(Debug)]
pub(crate) enum Working {
    /// Ours, and removed with the source.
    Temporary(Temporary),
    /// The caller's, and left alone — but claimed while the source lives.
    Named(Claimed),
}

impl Working {
    /// The directory, creating it if this is the first fetch.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created.
    pub(crate) fn path(&self) -> Result<&Path, Error> {
        match self {
            Self::Temporary(temporary) => Ok(temporary.path()),
            Self::Named(claimed) => {
                let path = claimed.path();

                if !path.exists() {
                    create_private(path).map_err(|error| {
                        Error::remote(format!(
                            "git: cannot create the working directory {}: {error}",
                            path.display()
                        ))
                    })?;
                }

                Ok(path)
            }
        }
    }
}

/// Records that this crate created `directory`, so it may empty it later.
///
/// # Errors
///
/// If the marker cannot be written. That is reported rather than shrugged off:
/// a working directory this crate can initialise but not write a file into is
/// one the next fetch is going to fail in anyway, and failing here says so
/// where the cause is visible.
pub(crate) fn mark(directory: &Path) -> Result<(), Error> {
    std::fs::write(directory.join(MARKER), MARKER_TEXT).map_err(|error| {
        Error::remote(format!(
            "git: cannot write {} in the working directory {}: {error}",
            MARKER,
            directory.display()
        ))
    })
}

/// Whether this crate created `directory` — the one thing that licenses a
/// delete.
fn ours(directory: &Path) -> bool {
    directory.join(MARKER).is_file()
}

/// How many packs the object database holds, which is how many fetches
/// transferred anything into it.
fn packs(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory.join("objects").join("pack")) else {
        return 0;
    };

    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "pack"))
        .count()
}

/// Empties `directory` when it has accumulated more than `after` transfers.
///
/// Returns whether anything was removed. See the [module
/// documentation](self#what-a-store-may-delete-and-where) for the rule this
/// enforces; the short version is that a directory without [`MARKER`] in it is
/// never touched, and `after` of `0` turns the whole thing off.
///
/// # Errors
///
/// If the directory is this crate's, is over the bound, and cannot be emptied.
/// A half-emptied object database is not a working one, so this is a failed
/// fetch rather than something to carry on past.
pub(crate) fn compact(directory: &Path, after: u32) -> Result<bool, Error> {
    if after == 0 || !ours(directory) || packs(directory) <= after as usize {
        return Ok(false);
    }

    empty(directory).map_err(|error| {
        Error::remote(format!(
            "git: cannot empty the working directory {}: {error}",
            directory.display()
        ))
    })?;

    Ok(true)
}

/// Removes everything in `directory`, leaving the directory itself.
///
/// The directory itself stays because the caller may have named it and may have
/// given it permissions or an ownership this crate should not be re-deciding —
/// and because removing it would race with anything holding it open. What it
/// held is rebuilt by the `init_bare` on the next line of the fetch.
fn empty(directory: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;

        // The entry's own type, which does not follow a link: a symlink planted
        // here is removed as a link rather than followed into whatever it
        // points at.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }

    Ok(())
}

/// A caller-named directory, held against a second source in this process.
pub(crate) struct Claimed {
    /// As the caller wrote it: what is opened, and what error messages say.
    path: PathBuf,
    /// What is actually held, by [`identity`]. Two spellings of one
    /// directory claim the same thing.
    identity: PathBuf,
}

impl Claimed {
    /// Claims `path`.
    ///
    /// # Errors
    ///
    /// If another live source in this process already named it — under any
    /// spelling. `cache` and `./cache` are one directory, and so are two
    /// symlinks to it; a lexical comparison would have let both through and
    /// given each its own fetch mutex, which is two fetches into one object
    /// database and a `compact` that can empty it while the other is
    /// reading.
    pub(crate) fn new(path: PathBuf) -> Result<Self, Error> {
        let identity = identity(&path);

        let mut claimed = claimed();
        let taken = claimed.get_or_insert_with(HashSet::new);

        if !taken.insert(identity.clone()) {
            return Err(Error::remote(format!(
                "git: {} is already the working directory of another source in \
                 this program; two sources fetching into one directory would \
                 corrupt it, so give each its own",
                path.display()
            )));
        }

        Ok(Self { path, identity })
    }

    fn path(&self) -> &Path {
        self.path.as_path()
    }
}

/// What two spellings of one directory have in common.
///
/// `canonicalize` where it can: it resolves `.`, `..`, a relative path
/// against the working directory, and every symlink on the way. It needs
/// the path to *exist*, though, and a working directory is routinely
/// claimed before it is created — so what does not exist yet is
/// canonicalized as far as it goes and the missing tail is appended.
///
/// Neither step is a security boundary and neither is asked to be. This is
/// a same-process bookkeeping question — which is why the answer is allowed
/// to fall back to the path as written when the filesystem will not answer
/// at all.
fn identity(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }

    let mut missing = Vec::new();
    let mut existing = path;

    while let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) {
        missing.push(name.to_owned());
        existing = parent;

        if let Ok(resolved) = existing.canonicalize() {
            let mut identity = resolved;

            for name in missing.iter().rev() {
                identity.push(name);
            }

            return identity;
        }
    }

    // Nothing on the path exists, so there is nothing to resolve against.
    // `absolute` still folds `./` and settles a relative path against the
    // working directory, which is the pair of spellings this is mostly
    // about; a failure leaves the path as the caller wrote it.
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

impl std::fmt::Debug for Claimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Claimed").field(&self.path).finish()
    }
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if let Some(taken) = claimed().as_mut() {
            taken.remove(&self.identity);
        }
    }
}

/// A directory this process made and will remove.
pub(crate) struct Temporary(PathBuf);

impl Temporary {
    /// Makes one, private, under the system temporary directory.
    ///
    /// # Errors
    ///
    /// If it cannot be created. The name carries the process id and a counter,
    /// so `create` — which is not recursive and therefore fails on an existing
    /// entry — is a claim rather than a race: a directory or symlink somebody
    /// planted at that path makes this fail rather than be adopted.
    pub(crate) fn new() -> Result<Self, Error> {
        let path = std::env::temp_dir().join(format!(
            "dynamic-config-git-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));

        create_private(&path).map_err(|error| {
            Error::remote(format!(
                "git: cannot create a working directory at {}: {error}",
                path.display()
            ))
        })?;

        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        self.0.as_path()
    }
}

impl std::fmt::Debug for Temporary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Temporary").field(&self.0).finish()
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        // Best effort: a directory that cannot be removed is a leaked
        // temporary directory, not a reason to panic in a `Drop`.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Creates `path`, readable only by this user, with no window in between.
///
/// Not recursive, on purpose: a recursive create silently adopts whatever is
/// already there, and what is already there might be a symlink into somebody
/// else's directory.
/// Two functions rather than one with a `#[cfg]` block inside it: the block
/// form leaves the `let mut builder` it needs unused on Windows, which is an
/// error under this workspace's `-D warnings` and one that reading the Unix
/// branch will never show you.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// **Windows has no `0700`**, and this crate does not pretend otherwise: the
/// directory inherits the parent's ACL. A deployment keeping fetched objects
/// on a shared Windows host should name a [`cache_dir`] whose permissions it
/// has already decided, rather than trusting a default this crate did not
/// set.
///
/// [`cache_dir`]: crate::Builder::cache_dir
#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temporary_directory_is_private_from_the_moment_it_exists() {
        let temporary = Temporary::new().unwrap();

        assert!(temporary.path().is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(temporary.path())
                .unwrap()
                .permissions()
                .mode();

            assert_eq!(
                mode & 0o777,
                0o700,
                "objects from a private repository must not be world-readable"
            );
        }
    }

    #[test]
    fn a_temporary_directory_is_removed_with_its_source() {
        let path = {
            let temporary = Temporary::new().unwrap();
            temporary.path().to_owned()
        };

        assert!(!path.exists(), "nothing survives the source");
    }

    #[test]
    fn two_sources_never_share_a_temporary_directory() {
        let one = Temporary::new().unwrap();
        let other = Temporary::new().unwrap();

        assert_ne!(one.path(), other.path());
    }

    #[test]
    fn a_named_directory_is_refused_to_a_second_source() {
        let directory = Temporary::new().unwrap();
        let path = directory.path().join("shared");

        let first = Claimed::new(path.clone()).expect("nobody has it yet");

        let error = Claimed::new(path.clone())
            .expect_err("two sources fetching into one directory would corrupt it");
        assert!(
            error.to_string().contains("already the working directory"),
            "{error}"
        );

        // ...and dropping the first source hands it back, so a program that
        // replaces a source can reuse the directory it paid to fill.
        drop(first);
        Claimed::new(path).expect("the claim ends with the source");
    }

    /// The claim is on a *directory*, not on a string. Two spellings of one
    /// path — and two symlinks to it — are one claim, because what they
    /// would share is one object database and one `compact` that can empty
    /// it under the other reader.
    #[test]
    fn one_directory_under_two_spellings_is_one_claim() {
        let directory = Temporary::new().unwrap();
        let path = directory.path().join("shared");
        std::fs::create_dir(&path).unwrap();

        let _first = Claimed::new(path.clone()).expect("nobody has it yet");

        // `dir/./shared` and `dir/shared/../shared` name what the first
        // source is already fetching into.
        for spelling in [
            directory.path().join(".").join("shared"),
            path.join("..").join("shared"),
        ] {
            Claimed::new(spelling.clone())
                .expect_err("the same directory, spelled differently, is the same directory");
        }

        // And through a symlink, which is the shape a deployment reaches by
        // accident rather than by writing `..`.
        #[cfg(unix)]
        {
            let link = directory.path().join("by-another-name");
            std::os::unix::fs::symlink(&path, &link).unwrap();

            Claimed::new(link).expect_err("a symlink to a claimed directory is that directory");
        }
    }

    /// A working directory is routinely claimed before it exists — the
    /// fetch creates it — so the identity has to survive a path the
    /// filesystem cannot resolve yet, and still see through the part of it
    /// that does exist.
    #[test]
    fn a_directory_that_does_not_exist_yet_can_still_be_claimed_once() {
        let directory = Temporary::new().unwrap();
        let path = directory.path().join("not-yet");

        let _first = Claimed::new(path.clone()).expect("nobody has it yet");

        Claimed::new(directory.path().join(".").join("not-yet"))
            .expect_err("the same directory that does not exist yet is still the same one");
    }

    /// Builds a directory that looks like an object database holding `count`
    /// packs, marked or not.
    fn a_working_directory(marked: bool, count: usize) -> Temporary {
        let directory = Temporary::new().unwrap();
        let packs = directory.path().join("objects").join("pack");

        std::fs::create_dir_all(&packs).unwrap();

        for index in 0..count {
            std::fs::write(packs.join(format!("pack-{index}.pack")), b"not really").unwrap();
            std::fs::write(packs.join(format!("pack-{index}.idx")), b"nor this").unwrap();
        }

        if marked {
            mark(directory.path()).unwrap();
        }

        directory
    }

    #[test]
    fn a_directory_under_the_bound_is_left_alone_and_one_over_it_is_emptied() {
        let directory = a_working_directory(true, 4);

        assert!(
            !compact(directory.path(), 4).unwrap(),
            "four packs is not more than four"
        );
        assert!(directory.path().join("objects").is_dir());

        let directory = a_working_directory(true, 5);

        assert!(compact(directory.path(), 4).unwrap(), "five is");
        assert!(
            !directory.path().join("objects").exists(),
            "the object database is what compaction removes"
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "and it leaves nothing behind but the directory itself"
        );
        assert!(directory.path().is_dir());
    }

    /// **The check that makes deleting defensible.** A caller who names a
    /// directory that is already a repository has not given this crate
    /// permission to empty it, and nothing here asks whether it looks empty
    /// enough.
    #[test]
    fn a_directory_this_crate_did_not_create_is_never_emptied() {
        let directory = a_working_directory(false, 50);

        assert!(
            !compact(directory.path(), 4).unwrap(),
            "no marker, no delete — at any size"
        );
        assert_eq!(
            std::fs::read_dir(directory.path().join("objects").join("pack"))
                .unwrap()
                .count(),
            100,
            "somebody else's packs are still there"
        );
    }

    #[test]
    fn compaction_can_be_turned_off_entirely() {
        let directory = a_working_directory(true, 50);

        assert!(
            !compact(directory.path(), 0).unwrap(),
            "`compact_after(0)` is a caller who runs their own `git gc`"
        );
        assert!(directory.path().join("objects").is_dir());
    }
}
