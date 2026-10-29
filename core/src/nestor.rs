//! What the service needs from the Nestor API (S1, S2, S4 of contract v2), and a fake server.
//!
//! The real HTTP client comes with the API work: the routes do not exist in Nestor yet. Until
//! then the live service has no [`NestorApi`] and answers `unavailable` to `associate`, and
//! [`FakeNestor`] stands in for the server in the tests and in the `--simulate` demo. The fake
//! applies the server-side rules the service relies on, so the tests show the service behaves
//! against them: hashed single-use device code, expiry, approval on the web only, proof of
//! possession of the machine key on every call, runner credentials good for one claim, revocation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::model::Queue;
use crate::persist::RevocationRequest;
use crate::secrets::Secret;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequest {
    /// X9.63 public key of the machine key, generated before this request.
    pub public_key: Vec<u8>,
    pub machine_name: String,
    pub model: String,
    pub operating_system: String,
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
    /// Poll less often (RFC 8628).
    SlowDown,
    Approved {
        machine_id: String,
        credential: Secret,
    },
    Denied,
    Expired,
}

/// Proof of possession of the machine key: a signature of `message`, which names the operation,
/// the subject and a timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub message: String,
    pub signature: Vec<u8>,
}

impl Proof {
    pub fn message(operation: &str, subject: &str, at_ms: u64) -> String {
        format!("nestor-machine\n{operation}\n{subject}\n{at_ms}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    Unreachable(String),
    /// The machine's credential was revoked: stop and erase everything.
    Revoked,
    Rejected(String),
}

/// What the Nestor instance announces it enforces itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The claim filters scopes atomically and never hands out a job of another scope (S6). Until
    /// it is announced, the service runs one task at a time (KCA-734 §3).
    pub scope_filter: bool,
}

pub trait NestorApi: Send + Sync {
    fn instance(&self) -> String;
    /// Nothing is announced by default: the routes of S6 do not exist yet.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn start_device_authorization(&self, request: &DeviceRequest) -> Result<DeviceAuthorization, ApiError>;
    fn poll_device_authorization(&self, device_code: &Secret, proof: &Proof) -> Result<DevicePoll, ApiError>;
    /// The eligible jobs per scope, capped, no content (S4).
    fn queue(&self, credential: &Secret, proof: &Proof) -> Result<Queue, ApiError>;
    /// A runner credential for one launch: short-lived, one claim (S2, A3). The server computes
    /// the scopes; the service never asks for one.
    fn issue_runner_credential(&self, credential: &Secret, proof: &Proof, runner_id: &str) -> Result<Secret, ApiError>;
    /// Signed before the key was erased; needs no secret.
    fn revoke(&self, request: &RevocationRequest) -> Result<(), ApiError>;
}

// MARK: Fake server

#[derive(Debug, Clone)]
struct PendingDevice {
    device_code_hash: u64,
    user_code: String,
    public_key: Vec<u8>,
    expires_at_ms: u64,
    approved: bool,
    denied: bool,
    consumed: bool,
}

#[derive(Debug, Clone)]
struct FakeMachine {
    public_key: Vec<u8>,
    credential: String,
    revoked: bool,
    runner_credentials: Vec<String>,
}

#[derive(Default)]
struct FakeState {
    now_ms: u64,
    pending: Vec<PendingDevice>,
    machines: BTreeMap<String, FakeMachine>,
    queue: Queue,
    unreachable: bool,
    scope_filter: bool,
    auto_approve: bool,
    next: u64,
    revocations: Vec<String>,
    rejected_proofs: u32,
}

/// A Nestor stand-in, in memory. **Test and demo only**: nothing it says is real.
pub struct FakeNestor {
    instance: String,
    state: Mutex<FakeState>,
}

/// Not a cryptographic hash: the fake only needs to show the device code is not kept in clear.
fn digest(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

impl FakeNestor {
    pub fn new(instance: &str) -> Self {
        FakeNestor { instance: instance.into(), state: Mutex::new(FakeState::default()) }
    }

    /// Approves every request at its first poll (the demo).
    pub fn auto_approve(self) -> Self {
        self.state.lock().unwrap().auto_approve = true;
        self
    }

    pub fn set_now(&self, now_ms: u64) {
        self.state.lock().unwrap().now_ms = now_ms;
    }

    /// An administrator approves `user_code` on the web app.
    pub fn approve(&self, user_code: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = state.now_ms;
        match state.pending.iter_mut().find(|p| p.user_code == user_code && !p.consumed && p.expires_at_ms > now) {
            Some(pending) => {
                pending.approved = true;
                true
            }
            None => false,
        }
    }

    pub fn deny(&self, user_code: &str) {
        if let Some(pending) = self.state.lock().unwrap().pending.iter_mut().find(|p| p.user_code == user_code) {
            pending.denied = true;
        }
    }

    pub fn set_queue(&self, queue: Queue) {
        self.state.lock().unwrap().queue = queue;
    }

    /// Announces (or not) the scope filter of S6.
    pub fn set_scope_filter(&self, scope_filter: bool) {
        self.state.lock().unwrap().scope_filter = scope_filter;
    }

    pub fn set_unreachable(&self, unreachable: bool) {
        self.state.lock().unwrap().unreachable = unreachable;
    }

    pub fn revoke_machine(&self, machine_id: &str) {
        if let Some(machine) = self.state.lock().unwrap().machines.get_mut(machine_id) {
            machine.revoked = true;
        }
    }

    pub fn is_revoked(&self, machine_id: &str) -> bool {
        self.state.lock().unwrap().machines.get(machine_id).is_some_and(|m| m.revoked)
    }

    pub fn revocations(&self) -> Vec<String> {
        self.state.lock().unwrap().revocations.clone()
    }

    pub fn rejected_proofs(&self) -> u32 {
        self.state.lock().unwrap().rejected_proofs
    }

    /// Credentials issued to runners of `machine_id`.
    pub fn runner_credentials(&self, machine_id: &str) -> Vec<String> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.runner_credentials.clone()).unwrap_or_default()
    }

    pub fn public_key_of(&self, machine_id: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.public_key.clone())
    }

    fn check(state: &mut FakeState) -> Result<(), ApiError> {
        if state.unreachable {
            Err(ApiError::Unreachable("connection refused".into()))
        } else {
            Ok(())
        }
    }

    fn verify(state: &mut FakeState, public_key: &[u8], proof: &Proof, operation: &str) -> Result<(), ApiError> {
        let valid = proof.message.split('\n').nth(1) == Some(operation)
            && crate::sys::verify_signature(public_key, proof.message.as_bytes(), &proof.signature);
        if valid {
            Ok(())
        } else {
            state.rejected_proofs += 1;
            Err(ApiError::Rejected("no valid proof of possession".into()))
        }
    }

    fn machine_for<'a>(
        state: &'a mut FakeState,
        credential: &Secret,
    ) -> Result<(String, &'a mut FakeMachine), ApiError> {
        let (id, machine) = state
            .machines
            .iter_mut()
            .find(|(_, m)| m.credential == credential.expose())
            .ok_or(ApiError::Rejected("unknown credential".into()))?;
        if machine.revoked {
            return Err(ApiError::Revoked);
        }
        Ok((id.clone(), machine))
    }
}

impl NestorApi for FakeNestor {
    fn instance(&self) -> String {
        self.instance.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { scope_filter: self.state.lock().unwrap().scope_filter }
    }

    fn start_device_authorization(&self, request: &DeviceRequest) -> Result<DeviceAuthorization, ApiError> {
        let mut state = self.state.lock().unwrap();
        Self::check(&mut state)?;
        if request.public_key.len() != 65 {
            return Err(ApiError::Rejected("a machine key is required".into()));
        }
        state.next += 1;
        let device_code = format!("nestor_device_{:032x}", digest(&format!("{}-{}", state.next, state.now_ms)));
        let user_code = format!("DEMO-{:04}", state.next % 10_000);
        let expires_at_ms = state.now_ms + 600_000;
        let approved = state.auto_approve;
        state.pending.push(PendingDevice {
            device_code_hash: digest(&device_code),
            user_code: user_code.clone(),
            public_key: request.public_key.clone(),
            expires_at_ms,
            approved,
            denied: false,
            consumed: false,
        });
        Ok(DeviceAuthorization {
            device_code: Secret::new(device_code),
            user_code,
            verification_uri: format!("https://{}/machines/approve", self.instance),
            expires_in_ms: 600_000,
            interval_ms: 5_000,
        })
    }

    fn poll_device_authorization(&self, device_code: &Secret, proof: &Proof) -> Result<DevicePoll, ApiError> {
        let mut state = self.state.lock().unwrap();
        Self::check(&mut state)?;
        let now = state.now_ms;
        let hash = digest(device_code.expose());
        let Some(index) = state.pending.iter().position(|p| p.device_code_hash == hash) else {
            return Ok(DevicePoll::Expired);
        };
        let pending = state.pending[index].clone();
        Self::verify(&mut state, &pending.public_key, proof, "poll")?;
        if pending.consumed || pending.expires_at_ms <= now {
            return Ok(DevicePoll::Expired);
        }
        if pending.denied {
            return Ok(DevicePoll::Denied);
        }
        if !pending.approved {
            return Ok(DevicePoll::Pending);
        }
        state.pending[index].consumed = true;
        state.next += 1;
        let machine_id = format!("00000000-0000-4000-8000-{:012x}", state.next);
        let credential = format!("nestor_machine_{:032x}", digest(&format!("{machine_id}-{now}-secret")));
        state.machines.insert(
            machine_id.clone(),
            FakeMachine {
                public_key: pending.public_key,
                credential: credential.clone(),
                revoked: false,
                runner_credentials: Vec::new(),
            },
        );
        Ok(DevicePoll::Approved { machine_id, credential: Secret::new(credential) })
    }

    fn queue(&self, credential: &Secret, proof: &Proof) -> Result<Queue, ApiError> {
        let mut state = self.state.lock().unwrap();
        Self::check(&mut state)?;
        let public_key = Self::machine_for(&mut state, credential)?.1.public_key.clone();
        Self::verify(&mut state, &public_key, proof, "queue")?;
        Ok(state.queue.clone())
    }

    fn issue_runner_credential(&self, credential: &Secret, proof: &Proof, runner_id: &str) -> Result<Secret, ApiError> {
        let mut state = self.state.lock().unwrap();
        Self::check(&mut state)?;
        let public_key = Self::machine_for(&mut state, credential)?.1.public_key.clone();
        Self::verify(&mut state, &public_key, proof, "runner-credential")?;
        state.next += 1;
        let token = format!("nestor_runner_{:032x}", digest(&format!("{runner_id}-{}", state.next)));
        Self::machine_for(&mut state, credential)?.1.runner_credentials.push(token.clone());
        Ok(Secret::new(token))
    }

    fn revoke(&self, request: &RevocationRequest) -> Result<(), ApiError> {
        let mut state = self.state.lock().unwrap();
        Self::check(&mut state)?;
        let Some(machine) = state.machines.get(&request.machine_id).cloned() else {
            return Ok(());
        };
        let message = RevocationRequest::message(&request.instance, &request.machine_id, request.issued_at_ms);
        let signature = crate::sys::unhex(&request.signature).unwrap_or_default();
        if !crate::sys::verify_signature(&machine.public_key, message.as_bytes(), &signature) {
            state.rejected_proofs += 1;
            return Err(ApiError::Rejected("invalid revocation signature".into()));
        }
        state.machines.get_mut(&request.machine_id).unwrap().revoked = true;
        state.revocations.push(request.machine_id.clone());
        Ok(())
    }
}
