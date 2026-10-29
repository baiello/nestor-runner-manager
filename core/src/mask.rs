//! Masking of secrets at the source of the service's logs (N5).
//!
//! Everything the service writes to a runner's log, to an event, to an error message of the
//! local API or to its own standard error goes through [`Redactor::mask`] first, so the `logs`
//! operation of the local API can only replay masked text. Masked: the secrets the service
//! handles (registered with [`Redactor::register`] as soon as they exist, unregistered never:
//! a secret that was once known stays masked), tokens with a known prefix, the credentials of
//! `Bearer` and `Basic`, the user and password of a URL, and the value after a name such as
//! `token` or `password`. Same rules as the Nestor runner's `mask.rs`; best effort on text.

use std::sync::{Arc, RwLock};

pub const MASK: &str = "[masked]";

/// Prefixes of tokens, with the fewest characters that must follow.
const PREFIXES: [(&str, usize); 17] = [
    ("nestor_runner_", 8),
    ("nestor_machine_", 8),
    ("nestor_enroll_", 8),
    ("nestor_device_", 8),
    ("github_pat_", 16),
    ("ghp_", 16),
    ("gho_", 16),
    ("ghs_", 16),
    ("sk-ant-", 16),
    ("sk-proj-", 16),
    ("sk-", 20),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("AKIA", 16),
    ("ya29.", 16),
    ("npm_", 30),
    ("eyJ", 30),
];

const NAMES: [&str; 11] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
    "credential",
    "device_code",
];

const HARMLESS: [&str; 6] = ["true", "false", "null", "none", "bearer", "basic"];

/// The masking applied to everything the service logs. Cheap to clone; clones share the set of
/// known secrets.
#[derive(Clone, Default)]
pub struct Redactor {
    known: Arc<RwLock<Vec<String>>>,
}

impl Redactor {
    pub fn new() -> Self {
        Redactor::default()
    }

    /// Masks `secret` wherever it appears from now on. Secrets shorter than 8 characters are
    /// ignored (they would mask ordinary words).
    pub fn register(&self, secret: &str) {
        if secret.len() < 8 {
            return;
        }
        let mut known = self.known.write().unwrap_or_else(|p| p.into_inner());
        if !known.iter().any(|k| k == secret) {
            known.push(secret.to_string());
            // Longest first, so a secret containing another one is masked whole.
            known.sort_by_key(|k| std::cmp::Reverse(k.len()));
        }
    }

    pub fn mask(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in self.known.read().unwrap_or_else(|p| p.into_inner()).iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), MASK);
            }
        }
        let text = mask_prefixed(&text);
        let text = mask_url_credentials(&text);
        let text = mask_schemes(&text);
        mask_named(&text)
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=' | '+' | '/')
}

fn token_length(text: &str) -> usize {
    text.find(|c: char| !is_token_char(c)).unwrap_or(text.len())
}

fn at_word_start(text: &str, index: usize) -> bool {
    text[..index].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
}

fn mask_prefixed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    'scan: while index < text.len() {
        if at_word_start(text, index) {
            for (prefix, minimum) in PREFIXES {
                if text[index..].starts_with(prefix) {
                    let length = token_length(&text[index + prefix.len()..]);
                    if length >= minimum {
                        out.push_str(MASK);
                        index += prefix.len() + length;
                        continue 'scan;
                    }
                }
            }
        }
        let c = text[index..].chars().next().unwrap_or_default();
        out.push(c);
        index += c.len_utf8();
    }
    out
}

fn mask_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find("://") {
        let start = position + 3;
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest
            .find(|c: char| c == '/' || c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ')'))
            .unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@') {
            if at > 0 {
                out.push_str(MASK);
                rest = &rest[at..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn mask_schemes(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    'scan: while index < text.len() {
        for scheme in ["bearer ", "basic "] {
            if lower[index..].starts_with(scheme) && at_word_start(text, index) {
                let start = index + scheme.len();
                let length = token_length(&text[start..]);
                if length >= 8 {
                    out.push_str(&text[index..start]);
                    out.push_str(MASK);
                    index = start + length;
                    continue 'scan;
                }
            }
        }
        let c = text[index..].chars().next().unwrap_or_default();
        out.push(c);
        index += c.len_utf8();
    }
    out
}

/// `name = value`, `name: value`, `"name": "value"`.
fn mask_named(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    'scan: while index < text.len() {
        for name in NAMES {
            if !lower[index..].starts_with(name) {
                continue;
            }
            let after_name = index + name.len();
            if text[after_name..].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            let rest = &text[after_name..];
            let skipped = rest.len() - rest.trim_start_matches(['"', '\'', ' ']).len();
            let rest = &rest[skipped..];
            let Some(separator) = rest.chars().next().filter(|c| matches!(c, '=' | ':')) else {
                continue;
            };
            let value_rest = &rest[separator.len_utf8()..];
            let spaces = value_rest.len() - value_rest.trim_start_matches([' ', '"', '\'']).len();
            let value_start = after_name + skipped + separator.len_utf8() + spaces;
            let length = token_length(&text[value_start..]);
            let value = &lower[value_start..value_start + length];
            if length == 0 || HARMLESS.contains(&value) || value == MASK.to_ascii_lowercase() {
                continue;
            }
            out.push_str(&text[index..value_start]);
            out.push_str(MASK);
            index = value_start + length;
            continue 'scan;
        }
        let c = text[index..].chars().next().unwrap_or_default();
        out.push(c);
        index += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secrets_and_known_forms_are_masked() {
        let redactor = Redactor::new();
        redactor.register("s3cr3t-runner-credential-value");
        redactor.register("short");
        let masked = redactor.mask(
            "got s3cr3t-runner-credential-value; header Bearer abcdefghijkl; url https://user:pw@nestor.example/x; \
             token=abc123456 \"password\": \"hunter22\" nestor_runner_0123456789abcdef ghp_0123456789abcdef0123",
        );
        assert!(!masked.contains("s3cr3t"), "{masked}");
        assert!(!masked.contains("abcdefghijkl"), "{masked}");
        assert!(!masked.contains("user:pw"), "{masked}");
        assert!(!masked.contains("abc123456"), "{masked}");
        assert!(!masked.contains("hunter22"), "{masked}");
        assert!(!masked.contains("nestor_runner_0123"), "{masked}");
        assert!(!masked.contains("ghp_0123"), "{masked}");
        assert!(masked.contains("nestor.example/x"), "{masked}");
    }

    #[test]
    fn ordinary_text_is_kept() {
        let redactor = Redactor::new();
        redactor.register("short");
        for text in [
            "Started (generation 3).",
            "Task t-42 finished: succeeded.",
            "tokens: 3 used",
            "token: true",
            "short words stay",
        ] {
            assert_eq!(redactor.mask(text), text);
        }
    }

    #[test]
    fn clones_share_known_secrets() {
        let redactor = Redactor::new();
        let clone = redactor.clone();
        redactor.register("abcdefgh12345678");
        assert_eq!(clone.mask("x abcdefgh12345678 y"), "x [masked] y");
    }
}
