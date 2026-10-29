//! Masking of secrets at the source of the service's logs (N5).
//!
//! Everything the service writes to a runner's log, to an event, to an error message of the
//! local API or to its own standard error goes through [`Redactor::mask`] first, so the `logs`
//! operation of the local API can only replay masked text. Masked: the secrets the service
//! handles, tokens with a known prefix, the credentials of `Bearer` and `Basic`, the user and
//! password of a URL, and the value after a name such as `token` or `password`. Same rules as the
//! Nestor runner's `mask.rs`; best effort on text.
//!
//! Known secrets come in two kinds (F4):
//! - **permanent** ([`Redactor::register`]): the few the service holds for long (device code,
//!   machine credential). Never forgotten.
//! - **transient** ([`Redactor::register_transient`]): one runner credential per launch. Each is
//!   forgotten [`FORGET_AFTER_MS`] after its runner stopped ([`Redactor::release`]), and at most
//!   [`TRANSIENT_CAPACITY`] are kept (least recently used dropped first), so memory and the cost
//!   of masking stay bounded however many runners are launched. A runner credential stays masked
//!   after that by its `nestor_runner_` prefix.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub const MASK: &str = "[masked]";

/// Runner credentials kept at most, least recently used dropped first.
pub const TRANSIENT_CAPACITY: usize = 256;
/// How long a runner credential stays known after its runner stopped.
pub const FORGET_AFTER_MS: u64 = 10 * 60 * 1_000;

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

/// A transient secret's registration, to [`Redactor::release`] it when its runner stops. Holds
/// no secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registration(u64);

struct Transient {
    handle: u64,
    secret: String,
    /// Set once released: forgotten at that time.
    forget_at_ms: Option<u64>,
}

#[derive(Default)]
struct Known {
    permanent: Vec<String>,
    /// Least recently used first.
    transient: VecDeque<Transient>,
    next_handle: u64,
}

impl Known {
    fn purge(&mut self, now_ms: u64) {
        self.transient.retain(|t| t.forget_at_ms.is_none_or(|at| at > now_ms));
        while self.transient.len() > TRANSIENT_CAPACITY {
            self.transient.pop_front();
        }
    }
}

/// The masking applied to everything the service logs. Cheap to clone; clones share the set of
/// known secrets.
#[derive(Clone)]
pub struct Redactor {
    known: Arc<Mutex<Known>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Default for Redactor {
    fn default() -> Self {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        Redactor::with_clock(Arc::new(move || start.elapsed().as_millis() as u64))
    }
}

impl Redactor {
    pub fn new() -> Self {
        Redactor::default()
    }

    /// With a clock in milliseconds (tests move it by hand).
    pub fn with_clock(clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Redactor { known: Arc::new(Mutex::new(Known::default())), clock }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Masks `secret` wherever it appears from now on, for good. Secrets shorter than 8
    /// characters are ignored (they would mask ordinary words).
    pub fn register(&self, secret: &str) {
        if secret.len() < 8 {
            return;
        }
        let mut known = self.known();
        if !known.permanent.iter().any(|k| k == secret) {
            known.permanent.push(secret.to_string());
        }
    }

    /// Masks a runner credential until [`FORGET_AFTER_MS`] after it is released, within
    /// [`TRANSIENT_CAPACITY`].
    pub fn register_transient(&self, secret: &str) -> Registration {
        let now = (self.clock)();
        let mut known = self.known();
        known.next_handle += 1;
        let handle = known.next_handle;
        if secret.len() >= 8 {
            known.transient.retain(|t| t.secret != secret);
            known.transient.push_back(Transient { handle, secret: secret.to_string(), forget_at_ms: None });
        }
        known.purge(now);
        Registration(handle)
    }

    /// The runner of this credential stopped: forget it [`FORGET_AFTER_MS`] from now.
    pub fn release(&self, registration: Registration) {
        let now = (self.clock)();
        let mut known = self.known();
        if let Some(entry) = known.transient.iter_mut().find(|t| t.handle == registration.0) {
            entry.forget_at_ms.get_or_insert(now + FORGET_AFTER_MS);
        }
        known.purge(now);
    }

    /// How many secrets are known (permanent, transient).
    pub fn known_counts(&self) -> (usize, usize) {
        let now = (self.clock)();
        let mut known = self.known();
        known.purge(now);
        (known.permanent.len(), known.transient.len())
    }

    pub fn mask(&self, text: &str) -> String {
        let mut text = text.to_string();
        {
            let now = (self.clock)();
            let mut known = self.known();
            known.purge(now);
            let mut secrets: Vec<(Option<u64>, String)> = known
                .permanent
                .iter()
                .map(|s| (None, s.clone()))
                .chain(known.transient.iter().map(|t| (Some(t.handle), t.secret.clone())))
                .collect();
            // Longest first, so a secret containing another one is masked whole.
            secrets.sort_by_key(|(_, s)| std::cmp::Reverse(s.len()));
            let mut used = Vec::new();
            for (handle, secret) in secrets {
                if text.contains(secret.as_str()) {
                    text = text.replace(secret.as_str(), MASK);
                    used.extend(handle);
                }
            }
            // Recently used credentials move to the back: the last to be dropped.
            for handle in used {
                if let Some(index) = known.transient.iter().position(|t| t.handle == handle) {
                    let entry = known.transient.remove(index).unwrap();
                    known.transient.push_back(entry);
                }
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
    fn runner_credentials_are_forgotten_after_their_runner_stops_and_stay_bounded() {
        let time = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clock_time = Arc::clone(&time);
        let redactor = Redactor::with_clock(Arc::new(move || clock_time.load(std::sync::atomic::Ordering::SeqCst)));
        redactor.register("machine-credential-0123456789");

        // 10 000 launches: memory stays bounded, whatever was not released.
        let mut last = None;
        for n in 0..10_000u32 {
            let registration = redactor.register_transient(&format!("credential-{n:08}-without-prefix"));
            if n % 2 == 0 {
                redactor.release(registration);
            }
            last = Some(registration);
            time.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
        }
        let (permanent, transient) = redactor.known_counts();
        assert_eq!(permanent, 1);
        assert!(transient <= TRANSIENT_CAPACITY, "{transient}");

        // The running runner's credential is masked; so is the permanent one.
        let masked = redactor.mask("x credential-00009999-without-prefix y machine-credential-0123456789");
        assert_eq!(masked, "x [masked] y [masked]");

        // Released, it is still masked for 10 minutes, then forgotten.
        let prefixed = redactor.register_transient("nestor_runner_abcdefgh12345678");
        let plain = redactor.register_transient("plain-credential-abcdefgh");
        redactor.release(prefixed);
        redactor.release(plain);
        redactor.release(last.unwrap());
        time.fetch_add(FORGET_AFTER_MS - 1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(redactor.mask("plain-credential-abcdefgh"), MASK);
        time.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(redactor.mask("plain-credential-abcdefgh"), "plain-credential-abcdefgh", "forgotten");
        // A runner credential stays masked for good by its prefix.
        assert_eq!(redactor.mask("token nestor_runner_abcdefgh12345678"), "token [masked]");
        assert_eq!(redactor.mask("machine-credential-0123456789"), MASK, "permanent secrets are never forgotten");
    }

    #[test]
    fn the_least_recently_used_credential_is_dropped_first() {
        let redactor = Redactor::new();
        redactor.register_transient("first-credential-0000");
        for n in 1..TRANSIENT_CAPACITY {
            redactor.register_transient(&format!("filler-credential-{n:04}"));
        }
        // Used just now: it moves to the back.
        assert_eq!(redactor.mask("first-credential-0000"), MASK);
        redactor.register_transient("one-more-credential-0000");
        assert_eq!(redactor.mask("first-credential-0000"), MASK, "recently used, kept");
        assert_eq!(redactor.mask("filler-credential-0001"), "filler-credential-0001", "least recently used, dropped");
    }

    #[test]
    fn clones_share_known_secrets() {
        let redactor = Redactor::new();
        let clone = redactor.clone();
        redactor.register("abcdefgh12345678");
        assert_eq!(clone.mask("x abcdefgh12345678 y"), "x [masked] y");
    }
}
