//! What a path segment may be, and what a key path may be.
//!
//! Narrow on purpose, and shared: [`is_name`] is what
//! [`ServerConfig::validate`](crate::ServerConfig) applies at startup, so a
//! section the handlers would refuse cannot be configured in the first
//! place.

/// An application or profile: a first character that is a letter or a digit,
/// then letters, digits, `.`, `_` and `-`, to 64.
///
/// Narrow on purpose. It bounds what can reach the audit log, it rejects
/// `..` and the empty segment without a special case for either, and there
/// is no application name anybody wants that it refuses.
///
/// `pub(crate)` so that [`ServerConfig::validate`](crate::ServerConfig)
/// refuses at startup exactly what the handlers refuse at request time: a
/// section this rejects would load, start and answer nothing.
pub(crate) fn is_name(value: &str) -> bool {
    let mut characters = value.chars();

    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 64
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A dotted key path: non-empty segments of letters, digits, `_` and `-`,
/// to 256 characters overall.
pub(super) fn is_key_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_narrow_and_rejects_traversal_and_newlines() {
        assert!(is_name("billing"));
        assert!(is_name("billing-api"));
        assert!(is_name("billing.api_2"));

        assert!(!is_name(""), "the empty segment");
        assert!(!is_name(".."), "traversal");
        assert!(!is_name(".hidden"), "a leading dot");
        assert!(!is_name("bill ing"), "a space");
        assert!(!is_name("billing\nadmin"), "a forged audit line");
        assert!(!is_name("bill/ing"), "a separator");
        assert!(!is_name(&"a".repeat(65)), "unbounded length");
    }

    #[test]
    fn a_key_path_is_dotted_and_has_no_empty_segments() {
        assert!(is_key_path("port"));
        assert!(is_key_path("pool.max_size"));
        assert!(is_key_path("a-b.c-d"));

        assert!(!is_key_path(""));
        assert!(!is_key_path("."));
        assert!(!is_key_path("pool."));
        assert!(!is_key_path(".pool"));
        assert!(!is_key_path("pool..max"));
        assert!(!is_key_path("pool max"));
        assert!(!is_key_path(&"a".repeat(257)));
    }
}
