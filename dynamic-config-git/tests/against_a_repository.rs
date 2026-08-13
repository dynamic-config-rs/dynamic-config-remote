//! Against real git repositories, made in a temporary directory.
//!
//! ```text
//! cargo test -p dynamic-config-git
//! ```
//!
//! Needs `git` on the path — which the crate needs anyway for the `file://`
//! and `ssh://` transports — and nothing else. No Docker, no network.

mod common;

use std::time::Duration;

use common::{Repository, Sandbox};
use dynamic_config::{ErrorKind, Format, RemoteSource, RemoteWatch, Value};
use dynamic_config_git::{Credential, GitSource, Keys};

/// A repository with one configuration file on `main`.
fn with_a_config(named: &str, contents: &str) -> (Sandbox, Repository) {
    let sandbox = Sandbox::new(named);
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit("services/api/config.yaml", contents);

    (sandbox, repository)
}

#[test]
fn a_file_is_read_from_the_branch_it_names() {
    let (sandbox, repository) = with_a_config("read", "app:\n  host: db.internal\n");

    let source = GitSource::builder(repository.url())
        .branch("main")
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let document = source.fetch().expect("the file is there");

    assert_eq!(document.text, "app:\n  host: db.internal\n");
    assert_eq!(
        document.format,
        Format::Yaml,
        "the format comes from the extension when nobody says otherwise"
    );
}

/// The provenance the crate promises: which commit is this program actually
/// serving? A branch name does not answer that, and it is the first question
/// of every configuration-in-git incident.
#[test]
fn a_fetched_document_names_the_commit_it_came_from() {
    let (sandbox, repository) = with_a_config("provenance", "app:\n  host: a\n");
    let commit = repository.head();

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(
        !source.describe().contains(&commit[..12]),
        "nothing has been read yet"
    );

    source.fetch().unwrap();

    let described = source.describe();

    assert!(described.contains(&commit[..12]), "{described}");
    assert!(
        described.contains("services/api/config.yaml"),
        "{described}"
    );
}

#[test]
fn a_moved_branch_is_what_the_next_fetch_returns() {
    let (sandbox, repository) = with_a_config("moved", "app:\n  host: a\n");

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(source.fetch().unwrap().text.contains("host: a"));

    repository.commit("services/api/config.yaml", "app:\n  host: b\n");

    assert!(
        source.fetch().unwrap().text.contains("host: b"),
        "a branch that moved is the point of watching one"
    );
}

/// The reload half, end to end: a watcher started against a branch delivers the
/// next commit and nothing before it.
#[test]
fn a_watcher_sees_the_commit_that_lands_after_it_started() {
    let (sandbox, repository) = with_a_config("watch", "app:\n  host: before\n");

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = std::sync::mpsc::channel();

    let watcher = std::thread::spawn(move || {
        source.watch(&watching, Duration::from_millis(100), move |document| {
            let _ = sender.send(document.text);

            Ok(())
        })
    });

    // The first tick records where the branch is without firing, so nothing
    // arrives until something is actually pushed.
    std::thread::sleep(Duration::from_millis(300));
    repository.commit("services/api/config.yaml", "app:\n  host: after\n");

    let delivered = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the watcher must notice a commit within a few poll intervals");

    assert!(delivered.contains("host: after"), "{delivered}");
    assert!(
        !delivered.contains("host: before"),
        "the value the caller already had must not be delivered as a change"
    );

    watch.stop();
    watcher.join().expect("the watcher thread").unwrap();
}

#[test]
fn a_tag_is_read_at_the_commit_it_points_to() {
    let (sandbox, repository) = with_a_config("tag", "app:\n  host: tagged\n");
    repository.tag("v1.0.0");

    // Move `main` past the tag, so reading the tag proves it is the tag being
    // read rather than whatever is newest.
    repository.commit("services/api/config.yaml", "app:\n  host: newer\n");

    let source = GitSource::builder(repository.url())
        .tag("v1.0.0")
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(source.fetch().unwrap().text.contains("host: tagged"));
}

#[test]
fn a_commit_id_pins_the_document_even_as_the_branch_moves() {
    let (sandbox, repository) = with_a_config("pinned", "app:\n  host: pinned\n");
    let pinned = repository.head();

    repository.allow_fetching_any_commit();
    repository.commit("services/api/config.yaml", "app:\n  host: newer\n");

    let source = GitSource::builder(repository.url())
        .commit(&pinned)
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert!(
        source.fetch().unwrap().text.contains("host: pinned"),
        "a pinned commit is reproducible or it is nothing"
    );
}

/// The three ways a caller gets it wrong, each named as itself. A message that
/// says "fetch failed" for all three is a message that costs a debugging
/// session.
#[test]
fn a_missing_ref_a_missing_file_and_a_missing_repository_are_three_errors() {
    let (sandbox, repository) = with_a_config("distinct", "app:\n  host: a\n");

    let no_such_branch = GitSource::builder(repository.url())
        .branch("release")
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache-branch"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("there is no `release`");

    assert!(
        no_such_branch.to_string().contains("no branch release"),
        "{no_such_branch}"
    );

    let no_such_file = GitSource::builder(repository.url())
        .path("services/api/missing.yaml")
        .cache_dir(sandbox.join("cache-file"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("there is no such file");

    assert!(
        no_such_file.to_string().contains("no such file"),
        "{no_such_file}"
    );
    assert!(
        no_such_file.to_string().contains("missing.yaml"),
        "the message names what was looked for: {no_such_file}"
    );

    let no_such_repository =
        GitSource::builder(format!("file://{}", sandbox.join("nowhere").display()))
            .path("services/api/config.yaml")
            .cache_dir(sandbox.join("cache-repo"))
            .build()
            .unwrap()
            .fetch()
            .expect_err("there is no repository there");

    assert_eq!(
        no_such_repository.kind(),
        ErrorKind::Remote,
        "a repository that is not there is not a credential problem"
    );
    assert!(
        no_such_repository.to_string().contains("nowhere"),
        "{no_such_repository}"
    );
}

/// The traversal case, which is the one this crate could uniquely get wrong.
///
/// A repository is untrusted input. A tree entry that is a symlink to
/// `../../../../etc/passwd` is the shape that makes a naive reader — one that
/// checks the repository out and then opens the configured path — read a file
/// outside the repository entirely. This crate never checks anything out and
/// refuses to follow the link, and the assertions below hold both halves:
/// the read fails, and the file the link named was never opened.
#[test]
#[cfg(unix)]
fn a_symlink_out_of_the_tree_is_refused_rather_than_followed() {
    let sandbox = Sandbox::new("symlink");
    let repository = Repository::create(sandbox.join("origin"));

    // A real file outside the repository, standing in for `/etc/shadow`.
    let outside = sandbox.join("outside.yaml");
    std::fs::write(&outside, "app:\n  host: stolen\n").unwrap();

    repository.commit_symlink("services/api/config.yaml", "../../../outside.yaml");

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let error = source
        .fetch()
        .expect_err("a symlink out of the tree must not be followed");

    assert!(error.to_string().contains("symbolic link"), "{error}");
    assert!(
        !error.to_string().contains("stolen"),
        "not one byte of the file it pointed at: {error}"
    );

    // And nothing was written where the link pointed, either: the working
    // directory is a bare object database and no checkout ever happens.
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "app:\n  host: stolen\n",
        "the file outside the repository must be untouched"
    );
    assert!(
        !sandbox.join("cache").join("services").exists(),
        "nothing from the tree is written to the working directory"
    );
}

#[test]
fn a_directory_where_a_file_was_expected_says_so() {
    let sandbox = Sandbox::new("directory");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit("services/api/config.yaml/inner.yaml", "app: {}\n");

    let error = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("a directory is not one document");

    assert!(error.to_string().contains("directory"), "{error}");
}

/// A two-gigabyte blob must be an error rather than an allocation. The size is
/// read from the object header, so the limit is enforced before any of the
/// file is loaded — which is why this test can use a small limit and a small
/// file and still be testing the thing that matters.
#[test]
fn a_file_over_the_limit_is_refused_from_its_header() {
    let (sandbox, repository) = with_a_config("oversize", &"x".repeat(4096));

    let error = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .max_bytes(1024)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("4096 bytes is over a 1024-byte limit");

    assert!(
        error.to_string().contains("over the 1024-byte limit"),
        "{error}"
    );
}

/// `max_bytes` bounds the whole read, not one file of it. It used to bound a
/// single file only, so a directory read was bounded at the file limit times
/// the key limit — half a gigabyte at the defaults, a number nobody chose.
/// Three files of 400-odd bytes are each under a 1024-byte limit and together
/// are over it.
#[test]
fn a_directory_read_is_bounded_in_total_rather_than_per_file() {
    let sandbox = Sandbox::new("total-budget");
    let repository = Repository::create(sandbox.join("origin"));
    let filler = "x".repeat(380);

    repository.commit_together(&[
        ("conf/a.yaml", &format!("a: \"{filler}\"\n")),
        ("conf/b.yaml", &format!("b: \"{filler}\"\n")),
        ("conf/c.yaml", &format!("c: \"{filler}\"\n")),
    ]);

    let error = GitSource::builder(repository.url())
        .path(Keys::prefix("conf"))
        .format(Format::Yaml)
        .max_bytes(1024)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("three 400-byte files are over a 1024-byte total");

    let rendered = error.to_string();

    assert!(
        rendered.contains("in total"),
        "the total budget should say so rather than reading like the per-file \
         limit: {error}"
    );
    assert!(
        !rendered.contains("xxx"),
        "a refusal names sizes, never contents: {error}"
    );
}

/// The same shape under a limit that fits, so the budget bounds a read rather
/// than forbidding directories.
#[test]
fn a_directory_that_fits_the_budget_is_read_whole() {
    let sandbox = Sandbox::new("total-budget-fits");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/a.yaml", "a: 1\n"),
        ("conf/b.yaml", "b: 2\n"),
        ("conf/c.yaml", "c: 3\n"),
    ]);

    let fetched = GitSource::builder(repository.url())
        .path(Keys::prefix("conf"))
        .format(Format::Yaml)
        .max_bytes(1024)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect("three small files are well under a 1024-byte total");

    let document = Value::parse(&fetched.text, fetched.format).unwrap();

    for key in ["a", "b", "c"] {
        assert!(document.get(key).is_some(), "{key} is missing");
    }
}

#[test]
fn a_file_that_is_not_utf8_is_an_error_rather_than_a_panic() {
    let sandbox = Sandbox::new("binary");
    let repository = Repository::create(sandbox.join("origin"));

    std::fs::create_dir_all(sandbox.join("origin").join("services")).unwrap();
    std::fs::write(
        sandbox.join("origin").join("services").join("config.json"),
        [0xff_u8, 0xfe, 0x00, 0x01],
    )
    .unwrap();

    // Committed through the helper's `add`, which does not care what is in it.
    repository.commit("services/marker", "");
    let output = std::process::Command::new("git")
        .current_dir(repository.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .args(["add", "--", "services/config.json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = std::process::Command::new("git")
        .current_dir(repository.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .args(["commit", "--quiet", "--message", "binary"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let error = GitSource::builder(repository.url())
        .path("services/config.json")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("no configuration format is arbitrary bytes");

    assert!(error.to_string().contains("not valid UTF-8"), "{error}");
}

/// The working directory is a cache: a source pointed at one that already has
/// the objects must not need the network to read them again. This is the
/// property that makes `cache_dir` worth naming.
#[test]
fn a_named_working_directory_is_reused_by_the_next_source() {
    let (sandbox, repository) = with_a_config("reuse", "app:\n  host: kept\n");
    let cache = sandbox.join("cache");

    {
        let first = GitSource::builder(repository.url())
            .path("services/api/config.yaml")
            .cache_dir(&cache)
            .build()
            .unwrap();

        first.fetch().unwrap();
    }

    // The origin is gone; only what the first source wrote into the working
    // directory is left, and the ref advertisement cannot happen at all.
    std::fs::remove_dir_all(repository.path()).unwrap();

    let second = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(&cache)
        .build()
        .unwrap();

    assert!(
        second.fetch().is_err(),
        "the origin is gone, so the ref cannot be resolved"
    );
    assert!(
        cache.join("objects").exists(),
        "the objects the first fetch paid for are still there"
    );
}

/// The planted-credential test, over a real fetch: a token in the remote URL
/// must not reach the error a failed fetch produces.
#[test]
fn a_token_in_the_url_never_reaches_a_failed_fetch_s_error() {
    let sandbox = Sandbox::new("planted");

    let error =
        GitSource::builder("https://x-access-token:ghs_hunter2@127.0.0.1:1/acme/config.git")
            .path("config.yaml")
            .credential(Credential::token("ghs_hunter2"))
            .with_timeout(Duration::from_secs(5))
            .cache_dir(sandbox.join("cache"))
            .build()
            .unwrap()
            .fetch()
            .expect_err("nothing is listening on port 1");

    assert!(!error.to_string().contains("hunter2"), "{error}");
    assert!(error.to_string().contains("x-access-token:***@"), "{error}");
}

/// SSH, end to end, without an `sshd`.
///
/// `gix` carries an SSH stream by spawning a program — the system `ssh`, or
/// whatever `core.sshCommand` names — and this crate's job is to put the right
/// command there. Pointing [`SshAuth::Command`] at a script that runs
/// `git upload-pack` locally exercises exactly that wiring: the `ssh://` url,
/// the command override, the transport, the shallow fetch and the blob read.
/// What it does not exercise is `sshd` itself, which is `ssh`'s business
/// rather than this crate's.
#[test]
#[cfg(unix)]
fn an_ssh_url_is_carried_by_the_command_the_credential_names() {
    use std::os::unix::fs::PermissionsExt;

    let (sandbox, repository) = with_a_config("ssh", "app:\n  host: over-ssh\n");

    // Stands in for `ssh`: the last argument is the command the remote would
    // have run, and running it here is what a working ssh connection amounts
    // to from the client's point of view.
    let fake_ssh = sandbox.join("fake-ssh");
    std::fs::write(
        &fake_ssh,
        // `-G` is the client asking `ssh` which variant it is before using it;
        // a program that fails that probe is not used. The argument after
        // `git-upload-pack` is the repository path as the *remote shell*
        // would receive it — quoted — so it goes through a shell here too.
        "#!/bin/sh\n\
         case \"$1\" in -G) exit 0 ;; esac\n\
         found=\n\
         for arg; do\n\
           if [ -n \"$found\" ]; then exec /bin/sh -c \"git upload-pack $arg\"; fi\n\
           [ \"$arg\" = git-upload-pack ] && found=1\n\
         done\n\
         echo \"no git-upload-pack in: $*\" >&2\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();

    let source = GitSource::builder(format!(
        "ssh://git@a-host-that-does-not-exist{}",
        repository.path().display()
    ))
    .path("services/api/config.yaml")
    .credential(Credential::ssh_command(fake_ssh.display().to_string()))
    .cache_dir(sandbox.join("cache"))
    .build()
    .unwrap();

    assert!(source.fetch().unwrap().text.contains("host: over-ssh"));
}

// ---------------------------------------------------------------------------
// Several files as one document
// ---------------------------------------------------------------------------

/// A named list is a precedence, exactly as a list of `.file(..)` calls is.
#[test]
fn a_named_list_merges_in_call_order_and_the_later_file_wins() {
    let sandbox = Sandbox::new("several");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/base.yaml", "db:\n  host: base\n  port: 5432\n"),
        ("conf/local.yaml", "db:\n  port: 6543\n"),
    ]);

    let source = GitSource::builder(repository.url())
        .path(Keys::several(["conf/base.yaml", "conf/local.yaml"]))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let document = source.fetch().expect("both files are there");
    let tree = Value::parse(&document.text, Format::Yaml).expect("the merge is a document");

    assert_eq!(tree.get("db.host"), Some(&Value::String("base".to_owned())));
    assert_eq!(tree.get("db.port"), Some(&Value::Integer(6543)));
}

/// Reversing the list reverses the answer, which is what makes the order a
/// promise rather than an accident of how the tree happened to be walked.
#[test]
fn reversing_the_list_reverses_which_file_wins() {
    let sandbox = Sandbox::new("reversed");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/base.yaml", "db:\n  port: 5432\n"),
        ("conf/local.yaml", "db:\n  port: 6543\n"),
    ]);

    let read = |paths: [&str; 2], cache: &str| {
        let source = GitSource::builder(repository.url())
            .path(Keys::several(paths))
            .cache_dir(sandbox.join(cache))
            .build()
            .unwrap();

        let document = source.fetch().unwrap();

        Value::parse(&document.text, Format::Yaml)
            .unwrap()
            .get("db.port")
            .cloned()
    };

    assert_eq!(
        read(["conf/base.yaml", "conf/local.yaml"], "one"),
        Some(Value::Integer(6543))
    );
    assert_eq!(
        read(["conf/local.yaml", "conf/base.yaml"], "other"),
        Some(Value::Integer(5432))
    );
}

/// A directory is disjoint sections, and it is read recursively.
#[test]
fn a_directory_folds_every_file_under_it_into_one_document() {
    let sandbox = Sandbox::new("prefix");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/db.yaml", "db:\n  host: db.internal\n"),
        ("conf/server.yaml", "server:\n  port: 8080\n"),
        ("conf/nested/cache.yaml", "cache:\n  ttl: 60\n"),
        // Outside the directory, and named so a string-prefix reader would
        // wrongly pick it up.
        ("conf-old.yaml", "db:\n  host: wrong\n"),
    ]);

    let source = GitSource::builder(repository.url())
        .path(Keys::prefix("conf"))
        .format(Format::Yaml)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let document = source.fetch().expect("three files, no overlap");
    let tree = Value::parse(&document.text, Format::Yaml).unwrap();

    assert_eq!(
        tree.get("db.host"),
        Some(&Value::String("db.internal".to_owned())),
        "a prefix is a directory, not a string prefix: {}",
        document.text
    );
    assert_eq!(tree.get("server.port"), Some(&Value::Integer(8080)));
    assert_eq!(
        tree.get("cache.ttl"),
        Some(&Value::Integer(60)),
        "the walk is recursive"
    );
}

/// The order a tree lists its entries in is nobody's precedence, so an overlap
/// under a directory is reported rather than resolved — naming both files and
/// the path, and never the values.
#[test]
fn two_files_under_one_directory_supplying_one_path_is_refused() {
    let sandbox = Sandbox::new("collision");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/db.yaml", "db:\n  password: hunter2-left\n"),
        ("conf/extra.yaml", "db:\n  password: hunter2-right\n"),
    ]);

    let error = GitSource::builder(repository.url())
        .path(Keys::prefix("conf"))
        .format(Format::Yaml)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("both files supply db.password");

    let printed = format!("{error} {error:?}");

    assert!(printed.contains("conf/db.yaml"), "{printed}");
    assert!(printed.contains("conf/extra.yaml"), "{printed}");
    assert!(printed.contains("db.password"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a collision report names paths and never values: {printed}"
    );
}

/// The same overlap in a list the caller wrote is not an error at all — the
/// list is the precedence.
#[test]
fn the_same_overlap_in_a_named_list_is_a_merge_rather_than_a_refusal() {
    let sandbox = Sandbox::new("overlap-list");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/db.yaml", "db:\n  host: left\n"),
        ("conf/extra.yaml", "db:\n  host: right\n"),
    ]);

    let source = GitSource::builder(repository.url())
        .path(Keys::several(["conf/db.yaml", "conf/extra.yaml"]))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let tree = Value::parse(&source.fetch().unwrap().text, Format::Yaml).unwrap();

    assert_eq!(
        tree.get("db.host"),
        Some(&Value::String("right".to_owned()))
    );
}

/// A file that is not there fails the whole fetch and names itself: a
/// configuration silently missing a section is worse than a refresh that failed
/// and left the last known good document serving.
#[test]
fn one_missing_file_fails_the_whole_read_and_names_it() {
    let sandbox = Sandbox::new("partial");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[("conf/db.yaml", "db:\n  host: a\n")]);

    let error = GitSource::builder(repository.url())
        .path(Keys::several(["conf/db.yaml", "conf/server.yaml"]))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("the second file was never committed");

    assert!(error.to_string().contains("conf/server.yaml"), "{error}");
    assert!(error.to_string().contains("no such file"), "{error}");
}

/// **The traversal case, for the walk.**
///
/// No porcelain will create a tree entry named `..`, but `git mktree` writes
/// what it is told and so does a pack arriving from a host nobody here
/// controls. A prefix read walks names the remote chose, so every one of them
/// is put back through the same check a configured path gets.
#[test]
fn a_tree_entry_that_tries_to_leave_the_directory_is_refused() {
    let sandbox = Sandbox::new("hostile-tree");
    let repository = Repository::create(sandbox.join("origin"));

    // A first ordinary commit, so the branch exists and `mktree` has a
    // repository to write into.
    repository.commit("conf/db.yaml", "db:\n  host: a\n");
    repository.commit_hostile_tree("conf", &[("..", "db:\n  host: escaped\n")]);

    let error = GitSource::builder(repository.url())
        .path(Keys::prefix("conf"))
        .format(Format::Yaml)
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap()
        .fetch()
        .expect_err("a tree entry named `..` must not be walked");

    assert!(
        error
            .to_string()
            .contains("not a file inside the repository"),
        "{error}"
    );
    assert!(
        !error.to_string().contains("escaped"),
        "not one byte of what it held: {error}"
    );
}

/// One file is still one file: byte for byte, comments and key order intact,
/// and no format feature needed that was not needed before.
#[test]
fn one_file_is_still_handed_over_exactly_as_it_was_committed() {
    let stored = "# the database\ndb:\n  zebra: 1\n  apple: 2\n";
    let (sandbox, repository) = with_a_config("byte-for-byte", stored);

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    assert_eq!(source.fetch().unwrap().text, stored);
}

/// Two paths naming two formats is caught where it was written, rather than
/// producing a syntax error about a file that has no syntax error in it.
#[test]
fn a_list_naming_two_formats_is_refused_at_build() {
    let error = GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::several(["conf/db.json", "conf/server.toml"]))
        .build()
        .expect_err("json and toml cannot both be it");

    assert!(error.to_string().contains("conf/db.json"), "{error}");
    assert!(error.to_string().contains("conf/server.toml"), "{error}");

    GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::several(["conf/db.json", "conf/server.toml"]))
        .format(Format::Json)
        .build()
        .expect("saying which one settles it");
}

/// A directory has no extension, so it needs `format` — and the error says so
/// rather than guessing.
#[test]
fn a_directory_needs_a_format_and_says_so() {
    let error = GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::prefix("conf"))
        .build()
        .expect_err("a directory names no format");

    assert!(
        error.to_string().contains("cannot tell what format"),
        "{error}"
    );
}

/// Every path in a list is checked the way one path always was.
#[test]
fn a_path_in_a_list_that_tries_to_leave_the_repository_is_refused() {
    let error = GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::several(["conf/db.yaml", "../../etc/shadow"]))
        .build()
        .expect_err("the second path leaves the repository");

    assert!(
        error
            .to_string()
            .contains("not a file inside the repository"),
        "{error}"
    );

    let error = GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::prefix("../../etc"))
        .format(Format::Yaml)
        .build()
        .expect_err("so does the directory");

    assert!(
        error
            .to_string()
            .contains("not a file inside the repository"),
        "{error}"
    );

    let error = GitSource::builder("https://github.com/acme/config.git")
        .path(Keys::several([] as [&str; 0]))
        .format(Format::Yaml)
        .build()
        .expect_err("a list of nothing is not a source");

    assert!(
        error.to_string().contains("name at least one file"),
        "{error}"
    );
}

/// **The property no other store in this family has.**
///
/// A watch on a set of keys is refused everywhere else, because waking on one
/// key and re-reading the rest collects a document that never existed. Here
/// what moves is a ref, what a ref names is a commit, and every file is read
/// out of that one commit's tree — so a deployment writing four files in one
/// commit is delivered as one document, and there is no interleaving to be had.
///
/// The assertion is the generation stamp: every delivery must carry the same
/// number in both files. A torn read is exactly a delivery where they differ.
#[test]
fn a_watch_on_several_files_never_delivers_a_document_that_never_existed() {
    let sandbox = Sandbox::new("atomic-watch");
    let repository = Repository::create(sandbox.join("origin"));

    repository.commit_together(&[
        ("conf/db.yaml", "db:\n  generation: 0\n"),
        ("conf/server.yaml", "server:\n  generation: 0\n"),
    ]);

    let source = GitSource::builder(repository.url())
        .path(Keys::several(["conf/db.yaml", "conf/server.yaml"]))
        .cache_dir(sandbox.join("cache"))
        .build()
        .unwrap();

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = std::sync::mpsc::channel();

    let watcher = std::thread::spawn(move || {
        source.watch(&watching, Duration::from_millis(50), move |document| {
            let _ = sender.send(document.text);

            Ok(())
        })
    });

    // Give the first tick time to record where the branch is without firing.
    std::thread::sleep(Duration::from_millis(200));

    for generation in 1..=6 {
        repository.commit_together(&[
            (
                "conf/db.yaml",
                &format!("db:\n  generation: {generation}\n"),
            ),
            (
                "conf/server.yaml",
                &format!("server:\n  generation: {generation}\n"),
            ),
        ]);

        std::thread::sleep(Duration::from_millis(60));
    }

    let mut seen = 0;
    let mut highest = 0;

    while let Ok(text) = receiver.recv_timeout(Duration::from_secs(10)) {
        let tree = Value::parse(&text, Format::Yaml).expect("every delivery is a document");

        let db = tree.get("db.generation").cloned();
        let server = tree.get("server.generation").cloned();

        assert_eq!(
            db, server,
            "a delivery whose two halves disagree is a document that never \
             existed at any commit: {text}"
        );

        if let Some(Value::Integer(generation)) = db {
            highest = highest.max(generation);
        }

        seen += 1;

        if highest == 6 {
            break;
        }
    }

    assert!(seen > 0, "the watcher must have delivered something");
    assert_eq!(highest, 6, "and must have caught up to the last commit");

    watch.stop();
    watcher.join().expect("the watcher thread").unwrap();
}

/// **The two-processes decision, as an assertion rather than a paragraph.**
///
/// This crate detects a second *source* naming one working directory and
/// refuses it, and deliberately does not detect a second *program* — a lock
/// file left by a killed process turns a fresh start into a hang, and the
/// deployment that shares a directory is one volume in two containers, where
/// neither pid liveness nor a clock is a thing to bound staleness with.
/// `working`'s module documentation argues that at length. What it promises in
/// exchange is that the damage is bounded, and this is that promise: the
/// directory is emptied underneath a live source, exactly as another program's
/// compaction would empty it, and the source loses one fetch rather than
/// itself. A watch loop waits out one interval and carries on.
#[test]
fn a_working_directory_emptied_by_another_program_costs_a_fetch_rather_than_the_source() {
    let (sandbox, repository) = with_a_config("shared-directory", "app:\n  host: first\n");
    let cache = sandbox.join("cache");

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(&cache)
        .build()
        .unwrap();

    assert!(source.fetch().unwrap().text.contains("first"));

    // What the other program's `compact` does to this one's objects.
    for entry in std::fs::read_dir(&cache).unwrap() {
        let entry = entry.unwrap();

        if entry.file_type().unwrap().is_dir() {
            std::fs::remove_dir_all(entry.path()).unwrap();
        } else {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }

    repository.commit("services/api/config.yaml", "app:\n  host: second\n");

    assert!(
        source.fetch().unwrap().text.contains("second"),
        "an emptied working directory is rebuilt by the next fetch, which is \
         what makes not detecting the second program survivable"
    );
}

/// The pruning half, end to end. `working::compact` has its own unit tests;
/// what this asserts is that the *fetch path calls it* — a shallow fetch of a
/// moving branch adds a pack every time the branch moves and removes nothing,
/// so a watcher left running for a month would otherwise grow without bound.
///
/// Thirty-five commits against a bound of thirty-two, so compaction has to
/// have happened at least once for the count to come back under it.
#[test]
fn a_long_lived_working_directory_does_not_grow_without_bound() {
    let (sandbox, repository) = with_a_config("compaction", "app:\n  n: 0\n");
    let cache = sandbox.join("cache");

    let source = GitSource::builder(repository.url())
        .path("services/api/config.yaml")
        .cache_dir(&cache)
        .build()
        .unwrap();

    for n in 1..=35 {
        repository.commit("services/api/config.yaml", &format!("app:\n  n: {n}\n"));
        source.fetch().expect("each commit is fetchable");
    }

    let packs = std::fs::read_dir(cache.join("objects").join("pack"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|it| it == "pack"))
                .count()
        })
        .unwrap_or(0);

    assert!(
        packs <= 33,
        "thirty-five transfers left {packs} packs; the fetch path is not \
         compacting"
    );

    // And the source still works afterwards — an emptied directory that
    // cannot be fetched into again would be a worse bug than growth.
    assert!(
        source.fetch().unwrap().text.contains("n: 35"),
        "a compacted directory must still be fetchable"
    );
}
