//! Vault Transit as a decryption stage.
//!
//! A configuration document in git whose passwords are ciphertext: the file
//! is reviewable in a pull request, diffable, greppable, and useless to
//! whoever clones the repository. That is the shape SOPS made popular, and
//! Vault's transit engine is the way to do it against a Vault a cluster
//! already runs.
//!
//! ```yaml
//! database:
//!   host: db.internal          # ordinary, and readable in review
//!   password: vault:v1:8SDd3W… # decrypted on the way into the pod
//! ```
//!
//! # Per value, not per document
//!
//! Encrypting the whole file would work and would throw away the reason to
//! do this at all: a file that is one opaque blob cannot be reviewed, and a
//! change to a port number looks exactly like a change to a password. So
//! this finds `vault:v1:…` tokens **wherever they appear** in the text and
//! replaces each with its plaintext, leaving everything around them alone.
//!
//! It is a text substitution rather than a parse. That is deliberate: the
//! document might be YAML, TOML, JSON, INI or `.properties`, the ciphertext
//! is a token none of those five can produce by accident, and a decryptor
//! that had to understand five formats would be five places for a format
//! to be understood slightly wrong.
//!
//! # One request, whatever the count
//!
//! Vault's transit engine takes a batch, so a document with forty encrypted
//! values costs one round trip rather than forty. A document with none
//! costs nothing at all — no request is made.

use std::collections::BTreeMap;

use dynamic_config::Error;
use dynamic_config_store_core::transform::Transform;

use crate::Vault;

/// The prefix every transit ciphertext carries.
///
/// Vault's own, and the reason a text substitution is safe: no YAML, TOML,
/// JSON, INI or `.properties` value produces this by accident.
const MARKER: &str = "vault:v1:";

/// Decrypts `vault:v1:…` values through a transit mount.
///
/// Holds a [`Vault`] for its client and its credential — the same
/// authentication, the same renewal, the same TLS configuration as the
/// store beside it, because a deployment that has told this project how to
/// reach Vault once should not have to do it twice.
pub struct Transit {
    vault: Vault,
    mount: String,
    key: String,
}

impl Transit {
    /// A decryptor against `key` in a transit `mount`.
    ///
    /// The `Vault` supplies the address, the credential and the TLS
    /// configuration; build it the way any Vault source is built and give
    /// it any path — this never reads a secret through it.
    #[must_use]
    pub fn new(vault: Vault, mount: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            vault,
            mount: mount.into(),
            key: key.into(),
        }
    }

    /// Every distinct ciphertext in the document, in the order found.
    ///
    /// Distinct because a document that names the same secret twice — a
    /// password and a connection string holding it — should cost one entry
    /// in the batch rather than two.
    fn ciphertexts(document: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let mut rest = document;

        while let Some(at) = rest.find(MARKER) {
            let from = &rest[at..];

            // A transit ciphertext is base64 with `+/=` in its alphabet;
            // it ends where the document's own punctuation begins.
            let end = from
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == ':')
                })
                .unwrap_or(from.len());

            let token = &from[..end];

            if token.len() > MARKER.len() && !found.iter().any(|held| held == token) {
                found.push(token.to_owned());
            }

            rest = &from[end.max(1)..];
        }

        found
    }

    /// One batch decrypt, and the plaintext for each ciphertext.
    fn decrypt(&self, ciphertexts: &[String]) -> Result<BTreeMap<String, String>, Error> {
        let url = format!(
            "{}/v1/{}/decrypt/{}",
            self.vault.address().trim_end_matches('/'),
            self.mount,
            self.key
        );

        let batch: Vec<serde_json::Value> = ciphertexts
            .iter()
            .map(|ciphertext| serde_json::json!({ "ciphertext": ciphertext }))
            .collect();

        let response = self.vault.post_authenticated(
            &url,
            serde_json::json!({ "batch_input": batch }),
            "the transit decrypt",
        )?;

        let results = response
            .get("data")
            .and_then(|data| data.get("batch_results"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                Error::remote(format!(
                    "{}: the transit decrypt answered without `data.batch_results`",
                    self.describe()
                ))
            })?;

        if results.len() != ciphertexts.len() {
            return Err(Error::remote(format!(
                "{}: asked to decrypt {} values and {} came back",
                self.describe(),
                ciphertexts.len(),
                results.len()
            )));
        }

        let mut plaintexts = BTreeMap::new();

        for (ciphertext, result) in ciphertexts.iter().zip(results) {
            // Vault reports a per-entry failure in the entry rather than in
            // the status, so a batch where one key is wrong answers 200.
            // Its `error` is Vault's own sentence about a key or a version
            // and carries no plaintext, which is why it is safe to quote.
            if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
                return Err(Error::remote(format!(
                    "{}: a value could not be decrypted: {error}",
                    self.describe()
                )));
            }

            let encoded = result
                .get("plaintext")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    Error::remote(format!(
                        "{}: a decrypt result carried no plaintext",
                        self.describe()
                    ))
                })?;

            // Transit answers base64, always — the engine encrypts bytes
            // and has no idea the caller meant text.
            let bytes = unbase64(encoded).ok_or_else(|| {
                Error::remote(format!(
                    "{}: a decrypted value is not base64",
                    self.describe()
                ))
            })?;

            let plaintext = String::from_utf8(bytes).map_err(|_| {
                // No lossy conversion: a value that is not text would be
                // written into a configuration file as replacement
                // characters, and the application would fail on something
                // that looks nothing like the cause.
                Error::remote(format!(
                    "{}: a decrypted value is not UTF-8; this stage writes \
                     text into a text document",
                    self.describe()
                ))
            })?;

            plaintexts.insert(ciphertext.clone(), plaintext);
        }

        Ok(plaintexts)
    }
}

impl Transform for Transit {
    fn apply(&self, document: &str) -> Result<String, Error> {
        let ciphertexts = Self::ciphertexts(document);

        // A document with nothing encrypted in it costs no request at all,
        // which is what makes this safe to leave in front of a store whose
        // documents are only sometimes sealed.
        if ciphertexts.is_empty() {
            return Ok(document.to_owned());
        }

        let plaintexts = self.decrypt(&ciphertexts)?;
        let mut readable = document.to_owned();

        // Longest first: `vault:v1:AAA` is a prefix of `vault:v1:AAAB`, and
        // replacing the short one first would leave the tail of the long
        // one behind as garbage.
        let mut order: Vec<&String> = plaintexts.keys().collect();
        order.sort_by_key(|ciphertext| std::cmp::Reverse(ciphertext.len()));

        for ciphertext in order {
            readable = readable.replace(ciphertext, &plaintexts[ciphertext]);
        }

        Ok(readable)
    }

    fn describe(&self) -> String {
        // The mount and the key name what this decrypts with. Neither is a
        // secret — they are policy objects, and naming them is what makes a
        // permission failure readable.
        format!("vault transit {}/{}", self.mount, self.key)
    }
}

/// Standard-alphabet base64, decoded.
///
/// Hand-rolled for the same reason the agent's is: this crate would
/// otherwise take a dependency to undo an encoding Vault applied on the way
/// out, and the alphabet has not moved since 1987.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };

    let clean: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    let mut bytes = Vec::with_capacity(clean.len() * 3 / 4);

    for chunk in clean.chunks(4) {
        let mut packed = 0u32;

        for byte in chunk {
            packed = (packed << 6) | value(*byte)?;
        }

        packed <<= 6 * (4 - chunk.len());

        for index in 0..chunk.len().saturating_sub(1) {
            bytes.push(((packed >> (16 - index * 8)) & 0xff) as u8);
        }
    }

    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_distinct_ciphertext_is_found_once() {
        let document = "host: db\npassword: vault:v1:AAAA\nurl: pg://x:vault:v1:AAAA@db\n";

        assert_eq!(Transit::ciphertexts(document), vec!["vault:v1:AAAA"]);
    }

    #[test]
    fn a_document_with_nothing_sealed_finds_nothing() {
        assert!(Transit::ciphertexts("host: db\nport: 5432\n").is_empty());
    }

    /// The token ends where the document's punctuation begins, whatever the
    /// format around it — which is why this is a text substitution rather
    /// than five parsers.
    #[test]
    fn a_ciphertext_ends_at_the_documents_own_punctuation() {
        for document in [
            "password = \"vault:v1:AbC+/9==\"\n",
            "{\"password\":\"vault:v1:AbC+/9==\"}",
            "password=vault:v1:AbC+/9==\n",
            "password: vault:v1:AbC+/9==\n",
        ] {
            assert_eq!(
                Transit::ciphertexts(document),
                vec!["vault:v1:AbC+/9=="],
                "{document}"
            );
        }
    }

    #[test]
    fn base64_round_trips_what_vault_sends() {
        assert_eq!(unbase64("aHVudGVyMg==").as_deref(), Some(&b"hunter2"[..]));
        assert_eq!(unbase64("").as_deref(), Some(&b""[..]));
        assert_eq!(unbase64("YQ==").as_deref(), Some(&b"a"[..]));
        assert_eq!(unbase64("YWI=").as_deref(), Some(&b"ab"[..]));
        assert_eq!(unbase64("YWJj").as_deref(), Some(&b"abc"[..]));
        assert!(unbase64("not base64!").is_none());
    }
}
