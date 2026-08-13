//! The one file this crate deliberately does *not* hold.
//!
//! `src/tls.rs` in `dynamic-config-consul`, `dynamic-config-firestore` and
//! `dynamic-config-vault` is one file, copied. The reasoning is written at the
//! top of all three and it is sound: the translation is `ureq`'s, and putting
//! `ureq` in this crate would put an HTTP client under etcd, NATS, Redis and
//! S3, none of which have one and none of which should grow one because three
//! siblings share a PEM parser.
//!
//! What was missing is the part that makes a copy safe to keep. The header
//! *asserts* the three are identical and nothing checked it, so a fix applied
//! to one — and a TLS translation is exactly the file that gets a security fix
//! — would have left the other two behind, silently, for as long as nobody
//! diffed them. This is that check, in the crate whose absence the copying is
//! justified by.
//!
//! It lives in `tests/`, which the workspace's `exclude` keeps out of every
//! published package: a crate downloaded from crates.io has no siblings to
//! compare itself with, and a test that cannot compile there would be a worse
//! problem than the one it solves.

/// The three copies are one file, byte for byte, as all three say they are.
///
/// A failure here is not "reformat something": it means a change landed in
/// one store's TLS translation and not in the others. Apply it to all three,
/// or — if they genuinely have to differ now — rewrite the headers that claim
/// they do not and delete this test in the same commit.
#[test]
fn the_three_ureq_tls_translations_are_one_file() {
    let consul = include_str!("../../dynamic-config-consul/src/tls.rs");

    for (crate_name, sibling) in [
        (
            "dynamic-config-firestore",
            include_str!("../../dynamic-config-firestore/src/tls.rs"),
        ),
        (
            "dynamic-config-vault",
            include_str!("../../dynamic-config-vault/src/tls.rs"),
        ),
    ] {
        assert_eq!(
            consul, sibling,
            "`{crate_name}/src/tls.rs` has drifted from \
             `dynamic-config-consul/src/tls.rs`, which all three files claim \
             to be identical to"
        );
    }
}
