//! What to present to a git host, and where it comes from.
//!
//! Every host in this crate's remit speaks git, and git has exactly two places
//! a credential can go: the HTTP `Authorization` header, or the `ssh` process
//! that carries the stream. [`Auth`] is those two, plus the absence of both.
//!
//! # The credential is a callable, not a string
//!
//! A store that takes `token: String` at construction works in a demo and
//! fails at three in the morning on the first refresh. A GitHub App
//! installation token lives one hour; a workload-identity token exchanged for
//! a provider token lives minutes; a watcher lives for the life of the
//! process. So [`Credential`] is a *function*, called per fetch, and the three
//! shapes it comes in are the three lifetimes a real credential has:
//!
//! | Constructor | Called | For |
//! |---|---|---|
//! | [`Credential::token`], [`basic`](Credential::basic), [`ssh_agent`](Credential::ssh_agent), [`ssh_key`](Credential::ssh_key), [`anonymous`](Credential::anonymous) | once | a value that cannot change |
//! | [`Credential::from_fn`] | every fetch | a value read from somewhere that can change — an environment variable, a file a sidecar rewrites |
//! | [`Credential::expiring`] | when it is about to expire | a value the issuer stamped a lifetime on |
//!
//! [`Credential::expiring`] is the one the item exists for. It is handed to
//! [`Cached`], the same machinery Vault, Consul and Firestore use: obtained
//! once, reused until it is within
//! [`REFRESH_WITHIN`](dynamic_config_store_core::credential::REFRESH_WITHIN) of
//! expiry, refreshed under one lock so eight threads produce one exchange, and
//! thrown away the moment the host refuses it so the next fetch obtains a new
//! one. None of that is re-derived here.
//!
//! # What is deliberately not here
//!
//! **The GitHub App JWT-to-installation-token exchange.** It is two steps — sign
//! an RS256 JWT with the app's private key, `POST
//! /app/installations/{id}/access_tokens` — and both belong in the caller's
//! closure. Signing needs an RSA implementation, and the pure-Rust one carries
//! an unpatched timing-sidechannel advisory that this workspace's `cargo deny`
//! gate rejects; a program that already talks to GitHub almost certainly has a
//! client that does the exchange. What this crate owes that flow is the
//! *refresh*, and that is [`Credential::expiring`].
//!
//! **An SSH key passphrase.** `ssh` has no way to accept one that does not put
//! it on a command line, where `ps` can read it, or in a file this crate would
//! have to write. A passphrase-protected key is therefore used through an
//! agent — `ssh-add` it once — which is what an agent is for. Taking a
//! passphrase parameter and then leaking it would be worse than not taking one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dynamic_config::Error;
use dynamic_config_store_core::credential::{Cached, Issued};

/// The user name GitHub, GitLab and Azure DevOps all accept beside a token.
///
/// HTTP basic authentication has two halves and a token is one value, so every
/// host picks a filler for the other. GitHub documents `x-access-token` for App
/// installation tokens and ignores the user name entirely for a personal access
/// token; GitLab and Azure DevOps ignore it too. One constant is therefore
/// correct everywhere, and [`Credential::basic`] is there for the host that
/// turns out not to be.
const TOKEN_USERNAME: &str = "x-access-token";

/// What to present to a git host.
///
/// Not `Debug`-derived: the HTTPS password is a token and the SSH command can
/// carry anything the caller put in it.
#[derive(Clone)]
#[non_exhaustive]
pub enum Auth {
    /// Nothing at all — a public repository over HTTPS.
    Anonymous,

    /// HTTP basic authentication, which is how every host takes a token.
    ///
    /// The token goes in `password`. The `username` half is filler that every
    /// host in this crate's remit ignores — `x-access-token` is what
    /// [`Credential::token`] puts there.
    Https {
        /// The user half, usually `x-access-token`.
        username: String,
        /// The secret half: a personal access token, an installation token, a
        /// deploy token, an OAuth bearer.
        password: String,
    },

    /// SSH, carried by the `ssh` program.
    Ssh(SshAuth),
}

impl Auth {
    /// How to name this method in an error, without naming its secret.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Anonymous => "anonymously",
            Self::Https { .. } => "an https token",
            Self::Ssh(SshAuth::Agent) => "an ssh agent",
            Self::Ssh(SshAuth::Key(_)) => "an ssh key",
            Self::Ssh(SshAuth::Command(_)) => "a custom ssh command",
        }
    }

    /// The `core.sshCommand` this method wants, if it is an SSH one.
    pub(crate) fn ssh_command(&self) -> Option<String> {
        match self {
            Self::Anonymous | Self::Https { .. } => None,
            Self::Ssh(ssh) => ssh.command(),
        }
    }
}

// Hand-written, never derived: a derive prints the password, and `{:?}`
// reaching a log is an ordinary accident — a `dbg!`, a
// `tracing::debug!(?source)`. The user name stays printable, because it is the
// half worth seeing when a host rejects the pair.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anonymous => f.write_str("Anonymous"),
            Self::Https { username, .. } => f
                .debug_struct("Https")
                .field("username", username)
                .field("password", &"***")
                .finish(),
            Self::Ssh(ssh) => f.debug_tuple("Ssh").field(ssh).finish(),
        }
    }
}

/// How the `ssh` program should be run.
///
/// SSH is not a Rust library here. `gix` carries an SSH stream by spawning the
/// system `ssh`, exactly as `git` does, so **the `ssh` binary must be on the
/// host** for any of these — and in exchange, everything already configured for
/// it works: `~/.ssh/config`, `known_hosts`, a `ProxyJump`, a hardware key.
#[derive(Clone)]
#[non_exhaustive]
pub enum SshAuth {
    /// Whatever `ssh` would do unaided: the agent in `SSH_AUTH_SOCK`, the keys
    /// `~/.ssh/config` names, the defaults.
    ///
    /// The right choice for a passphrase-protected key, because the agent is
    /// the only place a passphrase can be entered once and used many times.
    Agent,

    /// One private key file, and only that one.
    ///
    /// Adds `-o IdentitiesOnly=yes`, so an agent holding other keys cannot
    /// quietly offer them first and exhaust the server's `MaxAuthTries` before
    /// the intended key is tried. The path is a path; the key's *contents* are
    /// never read by this crate and never printed.
    Key(PathBuf),

    /// Run this instead of `ssh`.
    ///
    /// The escape hatch, and an explicit one — never a silent fallback. For a
    /// jump host, a vendored client, `-o` options this crate has no opinion
    /// about, or a test double. It becomes `core.sshCommand` for one fetch and
    /// is never written to the working directory's config.
    Command(String),
}

impl SshAuth {
    /// The `core.sshCommand` value for this method, if it needs one.
    fn command(&self) -> Option<String> {
        match self {
            // Nothing to say: `ssh` unaided is what git does anyway.
            Self::Agent => None,
            Self::Key(path) => Some(format!(
                "ssh -i {} -o IdentitiesOnly=yes",
                quoted(path.as_path())
            )),
            Self::Command(command) => Some(command.clone()),
        }
    }
}

/// A path as one shell word.
///
/// `core.sshCommand` is split by a shell-like parser, so a key under
/// `/home/my user/.ssh/id_ed25519` would otherwise become two arguments. Single
/// quotes with the POSIX `'\''` escape, because there is no other character a
/// single-quoted shell word treats specially.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => f.write_str("Agent"),
            // The path, not the key: a path is what a person debugging needs
            // and a key file's contents are never in this type to begin with.
            Self::Key(path) => f.debug_tuple("Key").field(path).finish(),
            // Redacted whole: a caller reaching for this hatch may well have
            // put `sshpass -p ...` in it, and this crate cannot tell.
            Self::Command(_) => f.write_str("Command(***)"),
        }
    }
}

/// Where an [`Auth`] comes from, and how long it lasts.
///
/// See the [module documentation](self) for which constructor to reach for.
#[derive(Clone)]
pub struct Credential(Kind);

#[derive(Clone)]
enum Kind {
    /// A value that cannot change; obtained once and kept.
    Constant(Auth),
    /// A value that may have changed since the last fetch.
    #[allow(clippy::type_complexity)]
    PerFetch(Arc<dyn Fn() -> Result<Auth, Error> + Send + Sync>),
    /// A value with a lifetime the issuer stamped on it.
    #[allow(clippy::type_complexity)]
    Expiring(Arc<dyn Fn(Option<&Auth>) -> Result<Issued<Auth>, Error> + Send + Sync>),
}

impl Credential {
    /// No credential at all — a public repository.
    #[must_use]
    pub fn anonymous() -> Self {
        Self(Kind::Constant(Auth::Anonymous))
    }

    /// A token that does not expire, over HTTPS.
    ///
    /// A classic personal access token, a GitLab deploy token, an Azure DevOps
    /// PAT. For one that *does* expire, see [`expiring`](Self::expiring) — a
    /// token pasted in here is presented unchanged forever, because there is
    /// nothing here to obtain another one with.
    #[must_use]
    pub fn token(token: impl Into<String>) -> Self {
        Self::basic(TOKEN_USERNAME, token)
    }

    /// A user name and password (or token) of the caller's choosing.
    ///
    /// For the host that does look at the user half — a GitLab deploy token is
    /// a real user name and a real token, and a CI job token is
    /// `gitlab-ci-token` plus `CI_JOB_TOKEN`.
    #[must_use]
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self(Kind::Constant(Auth::Https {
            username: username.into(),
            password: password.into(),
        }))
    }

    /// SSH through the agent in `SSH_AUTH_SOCK`, and whatever `~/.ssh/config`
    /// says.
    #[must_use]
    pub fn ssh_agent() -> Self {
        Self(Kind::Constant(Auth::Ssh(SshAuth::Agent)))
    }

    /// SSH with one named private key and no other.
    #[must_use]
    pub fn ssh_key(path: impl Into<PathBuf>) -> Self {
        Self(Kind::Constant(Auth::Ssh(SshAuth::Key(path.into()))))
    }

    /// SSH through a command of the caller's own.
    #[must_use]
    pub fn ssh_command(command: impl Into<String>) -> Self {
        Self(Kind::Constant(Auth::Ssh(SshAuth::Command(command.into()))))
    }

    /// A credential read afresh **on every fetch**.
    ///
    /// For a value that lives somewhere that can change without telling anyone
    /// — an environment variable a supervisor rewrites, a file a sidecar drops
    /// a new token into. Cheap, because a fetch is already a network round
    /// trip.
    ///
    /// ```
    /// # use dynamic_config_git::Credential;
    /// # use dynamic_config::Error;
    /// let credential = Credential::from_fn(|| {
    ///     let token = std::fs::read_to_string("/var/run/secrets/git-token")
    ///         .map_err(|error| Error::auth(format!("no git token: {error}")))?;
    ///
    ///     Ok(dynamic_config_git::Auth::Https {
    ///         username: "x-access-token".to_owned(),
    ///         password: token.trim().to_owned(),
    ///     })
    /// });
    /// ```
    #[must_use]
    pub fn from_fn(obtain: impl Fn() -> Result<Auth, Error> + Send + Sync + 'static) -> Self {
        Self(Kind::PerFetch(Arc::new(obtain)))
    }

    /// A credential the issuer stamped a lifetime on.
    ///
    /// The closure is handed the credential it is replacing — `None` on the
    /// first call and after a refusal — and returns the new one with the
    /// lifetime the issuer reported. It is called when there is nothing held,
    /// when what is held is within a minute of expiring, and immediately after
    /// the host refuses what was presented. It is *not* called per fetch: a
    /// GitHub App token exchange is a rate-limited API call, and one per poll
    /// tick would be a bill.
    ///
    /// ```no_run
    /// # use dynamic_config_git::{Auth, Credential};
    /// # use dynamic_config::Error;
    /// # use dynamic_config_store_core::credential::Issued;
    /// # use std::time::Duration;
    /// # fn installation_token() -> Result<(String, Duration), Error> { unimplemented!() }
    /// let credential = Credential::expiring(|_previous| {
    ///     // Sign the app JWT and exchange it for an installation token —
    ///     // whatever your GitHub client already does.
    ///     let (token, lives_for) = installation_token()?;
    ///
    ///     Ok(Issued {
    ///         value: Auth::Https {
    ///             username: "x-access-token".to_owned(),
    ///             password: token,
    ///         },
    ///         ttl: Some(lives_for),
    ///     })
    /// });
    /// ```
    #[must_use]
    pub fn expiring(
        obtain: impl Fn(Option<&Auth>) -> Result<Issued<Auth>, Error> + Send + Sync + 'static,
    ) -> Self {
        Self(Kind::Expiring(Arc::new(obtain)))
    }

    /// Whether a refused credential can be traded for a different one.
    ///
    /// A constant cannot: invalidating it would retry the identical string,
    /// which is one wasted round trip per fetch against a token that is simply
    /// wrong.
    fn is_replaceable(&self) -> bool {
        !matches!(self.0, Kind::Constant(_))
    }

    fn obtain(&self, previous: Option<&Auth>) -> Result<Issued<Auth>, Error> {
        match &self.0 {
            Kind::Constant(auth) => Ok(Issued {
                value: auth.clone(),
                ttl: None,
            }),
            // A zero lifetime is how "ask again next time" is spelled to
            // `Cached`: it is always inside the refresh margin, so every
            // `get` obtains. Cheaper than a second code path, and it keeps
            // the reactive `invalidate` working the same way for all three.
            Kind::PerFetch(obtain) => Ok(Issued {
                value: obtain()?,
                ttl: Some(Duration::ZERO),
            }),
            Kind::Expiring(obtain) => obtain(previous),
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Kind::Constant(auth) => f.debug_tuple("Constant").field(auth).finish(),
            Kind::PerFetch(_) => f.write_str("PerFetch(..)"),
            Kind::Expiring(_) => f.write_str("Expiring(..)"),
        }
    }
}

impl Default for Credential {
    fn default() -> Self {
        Self::anonymous()
    }
}

/// The credential currently held for one source, and when to get another.
///
/// The *when* is [`Cached`]'s, shared with the Vault, Consul and Firestore
/// crates. What is left here is the one thing git decides differently: there is
/// no renewal endpoint, so a stale credential is replaced rather than extended
/// and the `previous` argument exists only for a caller whose issuer can do
/// something with it.
#[derive(Debug)]
pub(crate) struct Session {
    credential: Credential,
    held: Cached<Auth>,
}

impl Session {
    pub(crate) fn new(credential: Credential) -> Self {
        Self {
            credential,
            held: Cached::new(),
        }
    }

    /// The credential to present, obtaining or refreshing it if it is time.
    pub(crate) fn current(&self) -> Result<Auth, Error> {
        self.held.get(|previous| self.credential.obtain(previous))
    }

    /// Throws away what is held, so the next [`current`](Self::current)
    /// obtains.
    pub(crate) fn invalidate(&self) {
        self.held.invalidate();
    }

    /// Whether trying again could present anything different.
    pub(crate) fn is_replaceable(&self) -> bool {
        self.credential.is_replaceable()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dynamic_config_store_core::credential::REFRESH_WITHIN;

    use super::*;

    #[test]
    fn a_constant_credential_is_obtained_once() {
        let session = Session::new(Credential::token("hunter2-token"));

        for _ in 0..3 {
            assert!(matches!(
                session.current().unwrap(),
                Auth::Https { password, .. } if password == "hunter2-token"
            ));
        }

        assert!(
            !session.is_replaceable(),
            "retrying the identical string is a wasted round trip"
        );
    }

    #[test]
    fn a_per_fetch_credential_is_read_every_time() {
        let calls = AtomicUsize::new(0);

        let session = Session::new(Credential::from_fn(move || {
            let count = calls.fetch_add(1, Ordering::SeqCst);

            Ok(Auth::Https {
                username: "x-access-token".to_owned(),
                password: format!("token-{count}"),
            })
        }));

        let seen: Vec<_> = (0..3)
            .map(|_| match session.current().unwrap() {
                Auth::Https { password, .. } => password,
                other => panic!("{other:?}"),
            })
            .collect();

        assert_eq!(seen, ["token-0", "token-1", "token-2"]);
    }

    /// The test the item exists for, at the credential seam: a token with an
    /// hour on it is reused, and one about to expire is replaced without the
    /// caller doing anything.
    #[test]
    fn an_expiring_credential_is_refreshed_before_it_dies_and_not_before() {
        let calls = AtomicUsize::new(0);
        let lifetimes = [REFRESH_WITHIN / 2, Duration::from_secs(3600)];

        let session = Session::new(Credential::expiring(move |_previous| {
            let count = calls.fetch_add(1, Ordering::SeqCst);

            Ok(Issued {
                value: Auth::Https {
                    username: "x-access-token".to_owned(),
                    password: format!("ghs_{count}"),
                },
                ttl: Some(lifetimes[count.min(1)]),
            })
        }));

        let password = |auth| match auth {
            Auth::Https { password, .. } => password,
            other => panic!("{other:?}"),
        };

        // Issued with half the margin left, so the next fetch replaces it...
        assert_eq!(password(session.current().unwrap()), "ghs_0");
        assert_eq!(password(session.current().unwrap()), "ghs_1");
        // ...and this one has an hour, so it is not replaced again.
        assert_eq!(password(session.current().unwrap()), "ghs_1");
        assert!(session.is_replaceable());
    }

    #[test]
    fn a_refused_credential_is_thrown_away_so_the_next_one_is_fresh() {
        let calls = AtomicUsize::new(0);

        let session = Session::new(Credential::expiring(move |previous| {
            assert!(
                previous.is_none(),
                "a credential the host refused must not be offered back for renewal"
            );

            Ok(Issued {
                value: Auth::Https {
                    username: "x-access-token".to_owned(),
                    password: format!("ghs_{}", calls.fetch_add(1, Ordering::SeqCst)),
                },
                ttl: Some(Duration::from_secs(3600)),
            })
        }));

        let password = |auth| match auth {
            Auth::Https { password, .. } => password,
            other => panic!("{other:?}"),
        };

        assert_eq!(password(session.current().unwrap()), "ghs_0");
        session.invalidate();
        assert_eq!(password(session.current().unwrap()), "ghs_1");
    }

    #[test]
    fn a_named_key_is_the_only_one_offered() {
        let command = SshAuth::Key(PathBuf::from("/home/app/.ssh/id_ed25519"))
            .command()
            .expect("a named key needs a command");

        assert_eq!(
            command,
            "ssh -i '/home/app/.ssh/id_ed25519' -o IdentitiesOnly=yes"
        );
        assert_eq!(
            SshAuth::Agent.command(),
            None,
            "the agent is what ssh does unaided"
        );
    }

    #[test]
    fn a_key_path_with_a_space_stays_one_argument() {
        let command = SshAuth::Key(PathBuf::from("/home/my user/.ssh/id_rsa"))
            .command()
            .unwrap();

        assert!(command.contains("'/home/my user/.ssh/id_rsa'"), "{command}");

        let command = SshAuth::Key(PathBuf::from("/home/o'brien/.ssh/id_rsa"))
            .command()
            .unwrap();

        assert!(
            command.contains(r"'/home/o'\''brien/.ssh/id_rsa'"),
            "{command}"
        );
    }

    #[test]
    fn debug_never_prints_a_credential() {
        let printed = format!(
            "{:?} {:?} {:?} {:?} {:?}",
            Credential::token("hunter2-token"),
            Credential::basic("gitlab-ci-token", "hunter2-job-token"),
            Credential::ssh_command("sshpass -p hunter2-passphrase ssh"),
            Credential::ssh_key("/home/app/.ssh/id_ed25519"),
            Credential::from_fn(|| Ok(Auth::Anonymous)),
        );

        assert!(!printed.contains("hunter2"), "{printed}");
        // The halves worth seeing survive: a user name and a key path are what
        // a person debugging a refused login actually needs.
        assert!(printed.contains("gitlab-ci-token"), "{printed}");
        assert!(printed.contains("id_ed25519"), "{printed}");
    }
}
