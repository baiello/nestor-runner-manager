//! What the service sends the Nestor API, as it goes on the wire, and how it reads the answers
//! (machine routes of nestor #156 and #157, proof `nestor-machine-proof/v2`).
//!
//! Every request of the machine is a [`MachineRequest`]: method, path and query, body, the
//! credential it authenticates with and its proof of possession. [`MachineClient`] builds and
//! signs them, one at a time, each signed strictly after the last, and reads the answers by
//! their status and error code, as `apps/api/README.md` ("Machines") of nestor describes them.
//! A [`NestorApi`] only carries a request to the server and brings its answer back.
//!
//! The live service has no [`NestorApi`] yet (no HTTP transport in this release): it answers
//! `unavailable` to `associate`. [`FakeNestor`] stands in for the server in the tests and in the
//! `--simulate` demo; it checks the proof exactly as nestor does.

mod fake;

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub use fake::FakeNestor;

use crate::model::{Mode, Queue, ServiceMode};
use crate::secrets::{MachineKey, Secret};
use crate::supervisor::Clock;

/// The first line of the signed message: the format of the proof.
pub const PROOF_FORMAT: &str = "nestor-machine-proof/v2";
/// The header the proof travels in: `<milliseconds since 1970>.<signature in base64url>`.
pub const PROOF_HEADER: &str = "Nestor-Machine-Proof";
/// How far from the server's clock a proof may be signed.
pub const PROOF_WINDOW_MS: u64 = 60_000;
/// The protocol of machines this service speaks (`machine_protocol_version`).
pub const PROTOCOL_VERSION: u32 = 1;
/// Times a request refused `stale_machine_proof` is signed again and sent again.
pub const STALE_RETRIES: usize = 3;
/// What the runners of this machine offer, as `nestor-runner run-once` claims (nestor
/// `apps/runner/src/client.rs`): the queue counts the jobs they can claim.
pub const OFFER_TOOLS: [&str; 1] = ["claude_code"];
pub const OFFER_KINDS: [&str; 3] = ["code", "work", "brief_draft"];

/// What a proof of `nestor-machine-proof/v2` signs: seven lines joined by `\n`, the format, the
/// audience (the API's origin), the method, the path with its query, the time, the base64url
/// SHA-256 of the secret the request authenticates with (empty without one) and that of the
/// body (that of nothing for an empty one).
pub fn proof_message(
    audience: &str,
    method: &str,
    target: &str,
    signed_at_ms: u64,
    secret: Option<&str>,
    body: &[u8],
) -> Vec<u8> {
    let secret = secret.map(|s| URL_SAFE_NO_PAD.encode(Sha256::digest(s.as_bytes()))).unwrap_or_default();
    let body = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
    format!("{PROOF_FORMAT}\n{audience}\n{method}\n{target}\n{signed_at_ms}\n{secret}\n{body}").into_bytes()
}

/// The header value of a proof.
pub fn proof_header(signed_at_ms: u64, signature: &[u8]) -> String {
    format!("{signed_at_ms}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The time and signature of a proof header, if it is well formed (as nestor reads it).
pub fn parse_proof_header(value: &str) -> Option<(u64, Vec<u8>)> {
    let (at, signature) = value.split_once('.')?;
    if at.is_empty() || !at.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    if signature.is_empty() || signature.len() > 72 {
        return None;
    }
    Some((at.parse().ok()?, signature))
}

/// The base64url form of a public key, as the association request sends it.
pub fn encode_public_key(public_key: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(public_key)
}

pub fn decode_public_key(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok().filter(|key| key.len() == 65 && key[0] == 4)
}

/// A request of the machine, as it goes on the wire.
#[derive(Debug, Clone)]
pub struct MachineRequest {
    pub method: &'static str,
    /// The path and the query, as sent.
    pub target: String,
    pub body: Vec<u8>,
    /// `Authorization: Bearer`: the machine credential, on the routes that take it.
    pub credential: Option<Secret>,
    /// The value of [`PROOF_HEADER`].
    pub proof: String,
}

/// The server's answer: its status, its body, and `Retry-After` in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after_s: Option<u64>,
}

impl MachineResponse {
    pub fn json(status: u16, body: Value) -> Self {
        MachineResponse { status, body: body.to_string().into_bytes(), retry_after_s: None }
    }

    pub fn error(status: u16, code: &str) -> Self {
        Self::json(status, json!({ "error": code }))
    }

    pub fn empty(status: u16) -> Self {
        MachineResponse { status, body: Vec::new(), retry_after_s: None }
    }

    /// The error code of the body, `{"error": "..."}`.
    pub fn code(&self) -> Option<String> {
        let value: Value = serde_json::from_slice(&self.body).ok()?;
        value.get("error")?.as_str().map(String::from)
    }
}

/// Carries a request to a Nestor instance and its answer back. `Err` only when the instance
/// could not be reached.
pub trait NestorApi: Send + Sync {
    /// The host of the instance (`nestor.example`); its API is `https://<instance>`.
    fn instance(&self) -> String;
    fn send(&self, request: &MachineRequest) -> Result<MachineResponse, String>;
}

// MARK: What the service asks and gets

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequest {
    /// X9.63 public key of the machine key, generated before this request.
    pub public_key: Vec<u8>,
    pub machine_name: String,
    pub model: String,
    /// `macos`, `linux`, `windows` or `other`.
    pub os: String,
    pub os_version: String,
    /// `aarch64`, `x86_64` or `other`.
    pub arch: String,
    /// `secure_enclave`; nestor refuses `software` (A6).
    pub key_storage: String,
}

#[derive(Debug)]
pub struct DeviceAuthorization {
    /// Secret: polled with, never shown, kept in memory only.
    pub device_code: Secret,
    /// Shown to the user, who types it on the Nestor web app.
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug)]
pub enum DevicePoll {
    Pending,
    /// Poll less often (RFC 8628): the server's new interval.
    SlowDown {
        interval_ms: Option<u64>,
    },
    Approved {
        machine_id: String,
        credential: Secret,
        /// Masked by the server (`o***r@example.com`): the account the machine would work for,
        /// and who approved it. Shown to the owner, who confirms them before any runner (M1).
        account_email: String,
        approved_by_email: String,
    },
    /// Approved, but the server holds it for now (`too_many_machines`, `machine_key_in_use`):
    /// polled again.
    Held(String),
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    Unreachable(String),
    /// The machine's credential is refused (revoked, or replaced): stop and erase everything.
    Revoked,
    /// `stale_machine_proof` again after [`STALE_RETRIES`] new signatures: the clock is off.
    StaleProof,
    Rejected(String),
}

/// What the Nestor instance announces it enforces itself (`capabilities`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Capabilities {
    /// The claim filters scopes atomically and never hands out a job of another scope (S6).
    #[serde(default)]
    pub scope_filter: bool,
    /// A runner of the machine may give back a job it must not run (N3).
    #[serde(default)]
    pub release: bool,
}

/// The leases the server applies to the runners of the machine (S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Leases {
    pub administrator_max: u32,
    pub paused_by_administrator: bool,
    pub max: u32,
    pub paused: bool,
    pub active: u64,
}

/// What the service reports every heartbeat (S5): known fields only, coarse measures.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatReport {
    pub service_mode: ServiceMode,
    pub mode: Mode,
    /// The jobs at once the service allows itself, 1 to 16.
    pub max_concurrent_tasks: u32,
    pub config_revision: u64,
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub memory_pressure: Option<&'static str>,
    pub disk_free_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HeartbeatAnswer {
    pub interval_seconds: u64,
    pub leases: Leases,
    pub capabilities: Capabilities,
}

/// The queued jobs a new runner of the machine may claim (S4): a count, and the opaque scope of
/// each, oldest first; nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct QueueAnswer {
    pub eligible: u32,
    pub capped: bool,
    pub scopes: Vec<String>,
    pub leases: Leases,
    pub capabilities: Capabilities,
}

impl QueueAnswer {
    /// The eligible jobs per scope, as the supervisor regulates on.
    pub fn queue(&self) -> Queue {
        let mut queue = Queue::empty();
        for scope in &self.scopes {
            *queue.eligible.entry(scope.clone()).or_default() += 1;
        }
        queue
    }
}

/// A runner of the machine on Nestor, and its token for one claim.
#[derive(Debug)]
pub struct IssuedRunner {
    pub runner_id: String,
    pub token: Secret,
}

// MARK: The client

/// Signs and sends the machine's requests, one at a time.
///
/// Nestor accepts a proof only if it is signed after the last one it accepted from the machine
/// (or from the request of association): two requests signed in the same millisecond, such as
/// the queue and then a runner's credential, would see the second refused. So every request is
/// signed and sent under one lock, at a time strictly later than the previous one, and a
/// `stale_machine_proof` is signed again with a new time and sent again, as its answer asks.
///
/// Lock order: the supervisor may call through this client while holding its own lock; this
/// type never calls the supervisor.
pub struct MachineClient {
    api: Arc<dyn NestorApi>,
    clock: Clock,
    /// The time of the last proof signed.
    last_signed_ms: Mutex<u64>,
}

/// The secret a request authenticates with, and how it is sent.
enum Auth<'a> {
    /// The request of association: no secret, its body carries the key.
    None,
    /// A poll or a cancellation: the device code, in the body, signed as the secret.
    DeviceCode(&'a Secret),
    /// `Authorization: Bearer` with the machine credential, signed as the secret.
    Credential(&'a Secret),
}

impl MachineClient {
    pub fn new(api: Arc<dyn NestorApi>, clock: Clock) -> Self {
        MachineClient { api, clock, last_signed_ms: Mutex::new(0) }
    }

    pub fn instance(&self) -> String {
        self.api.instance()
    }

    /// The audience of every proof: the origin of the API this service was configured with,
    /// never one read from an answer.
    pub fn audience(&self) -> String {
        format!("https://{}", self.api.instance())
    }

    fn call(
        &self,
        key: &dyn MachineKey,
        method: &'static str,
        target: &str,
        body: Option<&Value>,
        auth: Auth<'_>,
    ) -> Result<MachineResponse, ApiError> {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let audience = self.audience();
        let (secret, credential) = match auth {
            Auth::None => (None, None),
            Auth::DeviceCode(code) => (Some(code.expose()), None),
            Auth::Credential(credential) => (Some(credential.expose()), Some(credential.clone())),
        };
        let mut last = self.last_signed_ms.lock().unwrap();
        for _ in 0..=STALE_RETRIES {
            let signed_at_ms = (self.clock)().max(*last + 1);
            *last = signed_at_ms;
            let message = proof_message(&audience, method, target, signed_at_ms, secret, &body);
            let signature = key.sign(&message).map_err(ApiError::Rejected)?;
            let request = MachineRequest {
                method,
                target: target.into(),
                body: body.clone(),
                credential: credential.clone(),
                proof: proof_header(signed_at_ms, &signature),
            };
            let response = self.api.send(&request).map_err(ApiError::Unreachable)?;
            if response.status == 401 && response.code().as_deref() == Some("stale_machine_proof") {
                continue;
            }
            return Ok(response);
        }
        Err(ApiError::StaleProof)
    }

    /// The machine credential is refused, or the answer is not one of the route's.
    fn refused(response: &MachineResponse) -> ApiError {
        match (response.status, response.code().as_deref()) {
            (401, Some("invalid_machine_credential")) => ApiError::Revoked,
            (_, Some(code)) => ApiError::Rejected(code.to_string()),
            (status, None) => ApiError::Rejected(format!("HTTP {status}")),
        }
    }

    fn parse<T: for<'de> Deserialize<'de>>(response: &MachineResponse) -> Result<T, ApiError> {
        serde_json::from_slice(&response.body).map_err(|_| ApiError::Rejected("unexpected answer".into()))
    }

    /// `POST /machine/authorizations`, signed by the new key.
    pub fn request_authorization(
        &self,
        key: &dyn MachineKey,
        request: &DeviceRequest,
    ) -> Result<DeviceAuthorization, ApiError> {
        let body = json!({
            "name": request.machine_name,
            "model": request.model,
            "os": request.os,
            "os_version": request.os_version,
            "arch": request.arch,
            "key_algorithm": "ecdsa_p256_sha256",
            "public_key": encode_public_key(&request.public_key),
            "key_storage": request.key_storage,
        });
        let response = self.call(key, "POST", "/machine/authorizations", Some(&body), Auth::None)?;
        if response.status != 201 {
            return Err(Self::refused(&response));
        }
        #[derive(Deserialize)]
        struct Started {
            device_code: String,
            user_code: String,
            verification_uri: String,
            expires_in: u64,
            interval: u64,
        }
        let started: Started = Self::parse(&response)?;
        Ok(DeviceAuthorization {
            device_code: Secret::new(started.device_code),
            user_code: started.user_code,
            verification_uri: started.verification_uri,
            expires_in_ms: started.expires_in.saturating_mul(1_000),
            interval_ms: started.interval.saturating_mul(1_000),
        })
    }

    /// `POST /machine/authorizations/token`.
    pub fn poll(&self, key: &dyn MachineKey, device_code: &Secret) -> Result<DevicePoll, ApiError> {
        let body = json!({ "device_code": device_code.expose() });
        let response =
            self.call(key, "POST", "/machine/authorizations/token", Some(&body), Auth::DeviceCode(device_code))?;
        if response.status == 200 {
            #[derive(Deserialize)]
            struct Associated {
                machine_id: String,
                credential: String,
                account_email: String,
                approved_by_email: String,
            }
            let associated: Associated = Self::parse(&response)?;
            return Ok(DevicePoll::Approved {
                machine_id: associated.machine_id,
                credential: Secret::new(associated.credential),
                account_email: associated.account_email,
                approved_by_email: associated.approved_by_email,
            });
        }
        Ok(match (response.status, response.code().as_deref()) {
            (400, Some("authorization_pending")) => DevicePoll::Pending,
            (400, Some("slow_down")) => {
                DevicePoll::SlowDown { interval_ms: response.retry_after_s.map(|s| s.saturating_mul(1_000)) }
            }
            (400, Some("access_denied")) => DevicePoll::Denied,
            (400, Some("expired_token")) | (401, Some("invalid_device_code")) => DevicePoll::Expired,
            (409, Some(code @ ("too_many_machines" | "machine_key_in_use"))) => DevicePoll::Held(code.into()),
            _ => return Err(Self::refused(&response)),
        })
    }

    /// `POST /machine/authorizations/cancel`.
    pub fn cancel(&self, key: &dyn MachineKey, device_code: &Secret) -> Result<(), ApiError> {
        let body = json!({ "device_code": device_code.expose() });
        let response =
            self.call(key, "POST", "/machine/authorizations/cancel", Some(&body), Auth::DeviceCode(device_code))?;
        if response.status == 204 {
            Ok(())
        } else {
            Err(Self::refused(&response))
        }
    }

    /// `POST /machine/heartbeat` (S5).
    pub fn heartbeat(
        &self,
        key: &dyn MachineKey,
        credential: &Secret,
        report: &HeartbeatReport,
    ) -> Result<HeartbeatAnswer, ApiError> {
        let mut body = json!({
            "machine_protocol_version": PROTOCOL_VERSION,
            "service_mode": match report.service_mode { ServiceMode::Live => "live", ServiceMode::Demo => "demo" },
            "mode": match report.mode { Mode::Automatic => "automatic", Mode::Paused => "paused" },
            "max_concurrent_tasks": report.max_concurrent_tasks.clamp(1, 16),
            "config_revision": report.config_revision.min((1u64 << 53) - 1),
        });
        let object = body.as_object_mut().unwrap();
        if let Some(cpu) = report.cpu_percent {
            object.insert("cpu_percent".into(), json!(cpu.clamp(0.0, 100.0).round()));
        }
        if let Some(memory) = report.memory_percent {
            object.insert("memory_percent".into(), json!(memory.clamp(0.0, 100.0).round()));
        }
        if let Some(pressure) = report.memory_pressure {
            object.insert("memory_pressure".into(), json!(pressure));
        }
        if let Some(disk) = report.disk_free_bytes {
            object.insert("disk_free_bytes".into(), json!(disk.min(1u64 << 50)));
        }
        let response = self.call(key, "POST", "/machine/heartbeat", Some(&body), Auth::Credential(credential))?;
        if response.status != 200 {
            return Err(Self::refused(&response));
        }
        Self::parse(&response)
    }

    /// `POST /machine/queue` (S4), with what the runners offer.
    pub fn queue(&self, key: &dyn MachineKey, credential: &Secret) -> Result<QueueAnswer, ApiError> {
        let body = json!({ "tools": OFFER_TOOLS, "kinds": OFFER_KINDS, "models": true, "corrections": true });
        let response = self.call(key, "POST", "/machine/queue", Some(&body), Auth::Credential(credential))?;
        if response.status != 200 {
            return Err(Self::refused(&response));
        }
        Self::parse(&response)
    }

    /// `POST /machine/runners` with `{}`: a new runner of the machine and its token for one claim.
    /// No scope is ever asked for: the server computes it.
    pub fn create_runner(&self, key: &dyn MachineKey, credential: &Secret) -> Result<IssuedRunner, ApiError> {
        let response = self.call(key, "POST", "/machine/runners", Some(&json!({})), Auth::Credential(credential))?;
        if response.status != 201 && response.status != 200 {
            return Err(Self::refused(&response));
        }
        #[derive(Deserialize)]
        struct Created {
            runner_id: String,
            token: String,
        }
        let created: Created = Self::parse(&response)?;
        Ok(IssuedRunner { runner_id: created.runner_id, token: Secret::new(created.token) })
    }

    /// `POST /machine/runners/{id}/token`: a new token for one claim of a runner of the machine.
    pub fn runner_token(&self, key: &dyn MachineKey, credential: &Secret, runner_id: &str) -> Result<Secret, ApiError> {
        if !runner_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') || runner_id.is_empty() {
            return Err(ApiError::Rejected("invalid runner id".into()));
        }
        let target = format!("/machine/runners/{runner_id}/token");
        let response = self.call(key, "POST", &target, None, Auth::Credential(credential))?;
        if response.status != 200 && response.status != 201 {
            return Err(Self::refused(&response));
        }
        #[derive(Deserialize)]
        struct Issued {
            token: String,
        }
        Ok(Secret::new(Self::parse::<Issued>(&response)?.token))
    }

    /// `POST /machine/disassociate`: revokes the machine and its runners.
    pub fn disassociate(&self, key: &dyn MachineKey, credential: &Secret) -> Result<(), ApiError> {
        let response = self.call(key, "POST", "/machine/disassociate", None, Auth::Credential(credential))?;
        match response.status {
            204 => Ok(()),
            _ => Err(Self::refused(&response)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_nestors_seven_lines() {
        let message = proof_message("https://api.nestor.example.com", "POST", "/machine/runners?x=1", 42, None, b"");
        let text = String::from_utf8(message).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines,
            [
                "nestor-machine-proof/v2",
                "https://api.nestor.example.com",
                "POST",
                "/machine/runners?x=1",
                "42",
                "",
                // SHA-256 of nothing, base64url without padding.
                "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU",
            ]
        );
        let with_secret = proof_message("https://a", "POST", "/", 1, Some("nestor_machine_AAAA"), b"{}");
        let text = String::from_utf8(with_secret).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[5], URL_SAFE_NO_PAD.encode(Sha256::digest(b"nestor_machine_AAAA")));
        assert_eq!(lines[6], URL_SAFE_NO_PAD.encode(Sha256::digest(b"{}")));
    }

    #[test]
    fn a_header_reads_back_as_nestor_reads_it() {
        let header = proof_header(1_767_000_000_000, &[1, 2, 3]);
        assert_eq!(header, "1767000000000.AQID");
        assert_eq!(parse_proof_header(&header), Some((1_767_000_000_000, vec![1, 2, 3])));
        for invalid in ["", ".AQID", "12.", "-1.AQID", "1x.AQID", "12.AQID=", "12"] {
            assert_eq!(parse_proof_header(invalid), None, "{invalid}");
        }
    }
}
