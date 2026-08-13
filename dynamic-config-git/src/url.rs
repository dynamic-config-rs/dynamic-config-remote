//! Taking the credential back out of a remote URL.

use dynamic_config_store_core::LoneAuthority;

/// A git remote URL with any credential in it removed.
///
/// A git URL routinely carries one:
/// `https://x-access-token:ghs_abc@github.com/acme/config.git` is what every
/// CI system writes, and it is the string this crate quotes into every error
/// message and every `Debug`. The splitting is
/// [`dynamic_config_store_core::redacted`]'s, including the part that matters
/// here — it splits on the **last** `@`, because a token may contain one and
/// splitting on the first would leave its tail in the "redacted" output.
///
/// The one decision this crate makes is how to read an authority with no colon
/// in it, and for git it must be [`LoneAuthority::Secret`]:
/// `https://ghp_abc@github.com/acme/config.git` is a documented GitHub form in
/// which the whole authority **is** the token. Reading it as a user name would
/// print it. The cost is that `ssh://git@github.com/...` loses the `git`,
/// which is a word rather than information.
///
/// The `scp`-like spelling — `git@github.com:acme/config.git` — has no `://`
/// and so comes back unchanged. That is correct: the syntax has no place to
/// put a password, so there is nothing in it to remove.
pub(crate) fn redacted(url: &str) -> String {
    dynamic_config_store_core::redacted(url, LoneAuthority::Secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_in_a_remote_url_never_survives_redaction() {
        assert_eq!(
            redacted("https://x-access-token:ghs_hunter2@github.com/acme/config.git"),
            "https://x-access-token:***@github.com/acme/config.git"
        );

        // A GitHub token is base64-ish and can contain `@`. Splitting on the
        // first one would leave its tail in the message.
        assert_eq!(
            redacted("https://x-access-token:gh@s@hun@ter2@github.com/acme/config.git"),
            "https://x-access-token:***@github.com/acme/config.git"
        );

        // The whole authority is the token in this shape, so none of it may be
        // read as a printable user name.
        assert_eq!(
            redacted("https://ghp_hunter2@github.com/acme/config.git"),
            "https://***@github.com/acme/config.git"
        );

        // Azure DevOps, which puts an empty user before the PAT.
        assert_eq!(
            redacted("https://:hunter2-pat@dev.azure.com/acme/_git/config"),
            "https://:***@dev.azure.com/acme/_git/config"
        );
    }

    #[test]
    fn a_url_with_nothing_to_hide_is_left_alone() {
        assert_eq!(
            redacted("https://github.com/acme/config.git"),
            "https://github.com/acme/config.git"
        );
        // scp-like syntax cannot carry a password, so there is nothing to take
        // out and the user is worth seeing.
        assert_eq!(
            redacted("git@github.com:acme/config.git"),
            "git@github.com:acme/config.git"
        );
        assert_eq!(redacted("/srv/config.git"), "/srv/config.git");
    }
}
