//! Real git repositories, in a temporary directory.
//!
//! The honest test substrate for a git store is git. Every test here creates a
//! repository, commits into it and points a source at it over `file://` — no
//! network, no container, no mock of the thing under test. What that cannot
//! cover is authentication, because the file transport has none; that is what
//! the scripted git host in `over_http.rs` is for.

#![allow(dead_code)] // each test binary uses the part of this it needs

pub mod host;

use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory that is removed when the test ends.
pub struct Sandbox(PathBuf);

impl Sandbox {
    /// A fresh directory under the system temporary directory.
    pub fn new(named: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dynamic-config-git-test-{named}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        std::fs::create_dir_all(&path).expect("a temporary directory");

        Self(path)
    }

    pub fn path(&self) -> &Path {
        self.0.as_path()
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A git repository somebody could push configuration to.
pub struct Repository(PathBuf);

impl Repository {
    /// Initialises one at `path`, with `main` as its branch.
    pub fn create(path: PathBuf) -> Self {
        std::fs::create_dir_all(&path).expect("a repository directory");

        let repository = Self(path);
        repository.git(["init", "--initial-branch=main"]);

        repository
    }

    pub fn path(&self) -> &Path {
        self.0.as_path()
    }

    /// The `file://` url a source would name.
    pub fn url(&self) -> String {
        format!("file://{}", self.0.display())
    }

    /// Writes `contents` to `name` and commits it.
    pub fn commit(&self, name: &str, contents: &str) -> String {
        let file = self.0.join(name);

        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("a directory for the file");
        }

        std::fs::write(&file, contents).expect("writing the file");

        self.add_and_commit(name)
    }

    /// Commits a symlink at `name` pointing at `target` — the shape a
    /// repository would use to try to make a reader follow it out of the tree.
    #[cfg(unix)]
    pub fn commit_symlink(&self, name: &str, target: &str) -> String {
        let link = self.0.join(name);

        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).expect("a directory for the link");
        }

        std::os::unix::fs::symlink(target, &link).expect("creating the symlink");

        self.add_and_commit(name)
    }

    /// Writes several files and commits them **as one commit**.
    ///
    /// The shape a deployment actually has, and the one a multi-file source
    /// exists to read: four files that change together arrive together, so a
    /// reader either sees all four or none.
    pub fn commit_together(&self, files: &[(&str, &str)]) -> String {
        for (name, contents) in files {
            let file = self.0.join(name);

            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("a directory for the file");
            }

            std::fs::write(&file, contents).expect("writing the file");
            self.git(["add", "--", name]);
        }

        self.git(["commit", "--quiet", "--message", "a deployment"]);

        self.head()
    }

    /// Commits a tree built with `git mktree`, entry by entry.
    ///
    /// `git add` will not create an entry named `..` and neither will any
    /// porcelain — but `mktree` writes exactly what it is told, and so does a
    /// pack arriving from a host nobody here controls. This is how a hostile
    /// tree is produced for a test without a hostile host.
    ///
    /// `entries` are `(name, contents)` blobs, placed inside `directory`.
    pub fn commit_hostile_tree(&self, directory: &str, entries: &[(&str, &str)]) -> String {
        let listing: String = entries
            .iter()
            .map(|(name, contents)| {
                let blob = self.git_with_input(["hash-object", "-w", "--stdin"], contents);

                format!("100644 blob {blob}\t{name}\n")
            })
            .collect();

        let tree = self.git_with_input(["mktree"], &listing);
        let root = self.git_with_input(["mktree"], &format!("040000 tree {tree}\t{directory}\n"));
        let commit = self.git_with_input(["commit-tree", &root, "-m", "a hostile tree"], "");

        self.git(["update-ref", "refs/heads/main", &commit]);

        commit
    }

    fn add_and_commit(&self, name: &str) -> String {
        self.git(["add", "--", name]);
        self.git(["commit", "--quiet", "--message", &format!("add {name}")]);

        self.head()
    }

    /// Puts a tag on the current commit.
    pub fn tag(&self, name: &str) {
        self.git(["tag", "--annotate", name, "--message", "a release"]);
    }

    /// The commit `HEAD` points at, in full.
    pub fn head(&self) -> String {
        self.git(["rev-parse", "HEAD"])
    }

    /// Lets a client fetch an arbitrary object id, the way a hosted service
    /// does — `git` refuses by default over the local transport.
    pub fn allow_fetching_any_commit(&self) {
        self.git(["config", "uploadpack.allowAnySHA1InWant", "true"]);
    }

    fn git<const N: usize>(&self, arguments: [&str; N]) -> String {
        self.git_with_input(arguments, "")
    }

    fn git_with_input<const N: usize>(&self, arguments: [&str; N], input: &str) -> String {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new("git")
            .current_dir(&self.0)
            // The test must not depend on whoever is running it: a global
            // `insteadOf`, a signing key or a commit template would each make
            // this fail for one person and nobody else.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "dynamic-config tests")
            .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
            .env("GIT_COMMITTER_NAME", "dynamic-config tests")
            .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git must be installed to test a git store");

        child
            .stdin
            .take()
            .expect("a pipe")
            .write_all(input.as_bytes())
            .expect("writing to git");

        let output = child.wait_with_output().expect("git must finish");

        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout)
            .expect("git speaks utf-8")
            .trim()
            .to_owned()
    }
}
