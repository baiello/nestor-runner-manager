//! A Nestor stand-in, in memory. **Test and demo only**: nothing it says is real.
//!
//! It answers the machine routes of nestor #156 and #157 from the request as it would go on the
//! wire, and applies the server's rules the service relies on: the proof
//! `nestor-machine-proof/v2` checked exactly as nestor checks it (audience, method, path and
//! query, digests of the secret and of the body, within 60 s of its clock and strictly after
//! the last proof accepted for the machine or for the request of association), the device code
//! kept as a digest and single use, approval on the web only, runner tokens for one claim,
//! leases, revocation. Two departures, for the tests and the demo: it accepts a key in
//! software (memory keys), and the `demo` service mode, as nestor's own API tests do.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    decode_public_key, parse_proof_header, proof_message, MachineRequest, MachineResponse, NestorApi, PROOF_WINDOW_MS,
    PROTOCOL_VERSION,
};
use crate::model::Queue;

#[derive(Debug, Clone)]
struct PendingDevice {
    device_code_hash: String,
    user_code: String,
    public_key: Vec<u8>,
    name: String,
    key_storage: String,
    expires_at_ms: u64,
    approved: bool,
    denied: bool,
    consumed: bool,
    /// The time of the last proof accepted for this request: its own first.
    last_proof_ms: u64,
}

#[derive(Debug, Clone)]
struct FakeRunner {
    token: String,
    revoked: bool,
}

#[derive(Debug, Clone)]
struct FakeMachine {
    public_key: Vec<u8>,
    key_storage: String,
    credential: String,
    revoked: bool,
    last_proof_ms: u64,
    runners: BTreeMap<String, FakeRunner>,
    runner_credentials: Vec<String>,
    administrator_max: u32,
    paused_by_administrator: bool,
    declared_max: Option<u32>,
    declared_paused: bool,
    last_heartbeat_ms: Option<u64>,
    heartbeats: Vec<Value>,
}

#[derive(Default)]
struct FakeState {
    now_ms: u64,
    pending: Vec<PendingDevice>,
    machines: BTreeMap<String, FakeMachine>,
    /// Keys and times of the requests of association already recorded.
    request_proofs: Vec<(Vec<u8>, u64)>,
    queue: Queue,
    unreachable: bool,
    scope_filter: bool,
    auto_approve: bool,
    next: u64,
    revocations: Vec<String>,
    rejected_proofs: u32,
    stale_proofs: u32,
    /// Every request that reached the fake: method and target.
    requests: Vec<(String, String)>,
    /// Every proof accepted: its time.
    accepted: Vec<u64>,
    active_leases: u64,
}

pub struct FakeNestor {
    instance: String,
    state: Mutex<FakeState>,
}

const ACCOUNT_EMAIL: &str = "o***r@example.com";
const APPROVED_BY_EMAIL: &str = "a***n@example.com";

fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

enum ProofCheck {
    Valid(u64),
    Invalid,
    Stale,
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

    /// Announces (or not) `capabilities.scope_filter` and `capabilities.release` (S6, N3).
    pub fn set_scope_filter(&self, scope_filter: bool) {
        self.state.lock().unwrap().scope_filter = scope_filter;
    }

    /// What an administrator sets on the web app (`PATCH /machines/{id}`).
    pub fn set_administrator_limits(&self, machine_id: &str, max: u32, paused: bool) {
        if let Some(machine) = self.state.lock().unwrap().machines.get_mut(machine_id) {
            machine.administrator_max = max;
            machine.paused_by_administrator = paused;
        }
    }

    /// The jobs the machine's runners hold, as `leases.active` reports it.
    pub fn set_active_leases(&self, active: u64) {
        self.state.lock().unwrap().active_leases = active;
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

    /// The machines that disassociated themselves.
    pub fn revocations(&self) -> Vec<String> {
        self.state.lock().unwrap().revocations.clone()
    }

    /// Proofs refused as not a signature of the request by the key.
    pub fn rejected_proofs(&self) -> u32 {
        self.state.lock().unwrap().rejected_proofs
    }

    /// Proofs refused `stale_machine_proof`: out of the window, or not after the last accepted.
    pub fn stale_proofs(&self) -> u32 {
        self.state.lock().unwrap().stale_proofs
    }

    /// The times of the proofs accepted, in the order they were.
    pub fn accepted_proofs(&self) -> Vec<u64> {
        self.state.lock().unwrap().accepted.clone()
    }

    /// Every request that reached the fake, as `METHOD target`.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.iter().map(|(m, t)| format!("{m} {t}")).collect()
    }

    /// The heartbeats `machine_id` sent, as their bodies.
    pub fn heartbeats(&self, machine_id: &str) -> Vec<Value> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.heartbeats.clone()).unwrap_or_default()
    }

    /// Tokens issued to runners of `machine_id`.
    pub fn runner_credentials(&self, machine_id: &str) -> Vec<String> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.runner_credentials.clone()).unwrap_or_default()
    }

    /// The runners of `machine_id` on Nestor.
    pub fn runners_of(&self, machine_id: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .machines
            .get(machine_id)
            .map(|m| m.runners.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn public_key_of(&self, machine_id: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.public_key.clone())
    }

    pub fn key_storage_of(&self, machine_id: &str) -> Option<String> {
        self.state.lock().unwrap().machines.get(machine_id).map(|m| m.key_storage.clone())
    }

    /// Whether `token` is the current token of a runner of a machine that is not revoked.
    pub fn accepts_runner_token(&self, token: &str) -> bool {
        let state = self.state.lock().unwrap();
        state
            .machines
            .values()
            .filter(|m| !m.revoked)
            .any(|m| m.runners.values().any(|r| !r.revoked && r.token == token))
    }

    /// The machine of a runner token.
    pub fn machine_of_runner_token(&self, token: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.machines.iter().find(|(_, m)| m.runners.values().any(|r| r.token == token)).map(|(id, _)| id.clone())
    }

    // MARK: The proof

    fn audience(&self) -> String {
        format!("https://{}", self.instance)
    }

    /// Checks the proof of `request` as nestor does, for `secret` and `public_key`, and that it
    /// is signed after `last`, which it updates. A proof out of the window records nothing.
    fn check_proof(
        &self,
        state: &mut FakeState,
        request: &MachineRequest,
        secret: Option<&str>,
        public_key: &[u8],
        last: Option<u64>,
    ) -> ProofCheck {
        let Some((at, signature)) = parse_proof_header(&request.proof) else {
            state.rejected_proofs += 1;
            return ProofCheck::Invalid;
        };
        let message = proof_message(&self.audience(), request.method, &request.target, at, secret, &request.body);
        if !crate::sys::verify_signature(public_key, &message, &signature) {
            state.rejected_proofs += 1;
            return ProofCheck::Invalid;
        }
        if state.now_ms.abs_diff(at) > PROOF_WINDOW_MS || last.is_some_and(|last| at <= last) {
            state.stale_proofs += 1;
            return ProofCheck::Stale;
        }
        state.accepted.push(at);
        ProofCheck::Valid(at)
    }

    fn proof_refused(check: &ProofCheck) -> MachineResponse {
        match check {
            ProofCheck::Stale => MachineResponse::error(401, "stale_machine_proof"),
            _ => MachineResponse::error(401, "invalid_machine_proof"),
        }
    }

    fn leases(state: &FakeState, machine: &FakeMachine) -> Value {
        json!({
            "administrator_max": machine.administrator_max,
            "paused_by_administrator": machine.paused_by_administrator,
            "max": machine.administrator_max.min(machine.declared_max.unwrap_or(1)),
            "paused": machine.paused_by_administrator || machine.declared_paused,
            "active": state.active_leases,
        })
    }

    fn capabilities(state: &FakeState) -> Value {
        json!({ "scope_filter": state.scope_filter, "release": state.scope_filter })
    }

    // MARK: Routes

    fn route(&self, request: &MachineRequest) -> MachineResponse {
        let mut state = self.state.lock().unwrap();
        state.requests.push((request.method.into(), request.target.clone()));
        if request.method != "POST" {
            return MachineResponse::error(405, "method_not_allowed");
        }
        let body: Value = if request.body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(_) => return MachineResponse::error(400, "invalid_request"),
            }
        };
        match request.target.as_str() {
            "/machine/authorizations" => self.authorize(&mut state, request, &body),
            "/machine/authorizations/token" => self.poll(&mut state, request, &body),
            "/machine/authorizations/cancel" => self.cancel(&mut state, request, &body),
            target if target.starts_with("/machine/") => {
                let Some(credential) = request.credential.as_ref().map(|c| c.expose().to_string()) else {
                    return MachineResponse::error(401, "invalid_machine_credential");
                };
                let Some((id, machine)) = state.machines.iter().find(|(_, m)| m.credential == credential) else {
                    return MachineResponse::error(401, "invalid_machine_credential");
                };
                if machine.revoked {
                    return MachineResponse::error(401, "invalid_machine_credential");
                }
                let (id, key, last) = (id.clone(), machine.public_key.clone(), machine.last_proof_ms);
                match self.check_proof(&mut state, request, Some(&credential), &key, Some(last)) {
                    ProofCheck::Valid(at) => state.machines.get_mut(&id).unwrap().last_proof_ms = at,
                    check => return Self::proof_refused(&check),
                }
                self.machine_route(&mut state, &id, target, &body)
            }
            _ => MachineResponse::error(404, "not_found"),
        }
    }

    fn authorize(&self, state: &mut FakeState, request: &MachineRequest, body: &Value) -> MachineResponse {
        let text = |name: &str| body.get(name).and_then(Value::as_str);
        let (Some(name), Some(Some(public_key)), Some("ecdsa_p256_sha256"), Some(key_storage)) =
            (text("name"), text("public_key").map(decode_public_key), text("key_algorithm"), text("key_storage"))
        else {
            return MachineResponse::error(422, "invalid_request");
        };
        if !matches!(key_storage, "secure_enclave" | "software") {
            return MachineResponse::error(422, "invalid_request");
        }
        let at = match self.check_proof(state, request, None, &public_key, None) {
            ProofCheck::Valid(at) => at,
            check => return Self::proof_refused(&check),
        };
        // One request per key and time of signing: the unique index of nestor.
        if state.request_proofs.iter().any(|(key, time)| *key == public_key && *time == at) {
            state.stale_proofs += 1;
            return MachineResponse::error(401, "stale_machine_proof");
        }
        state.request_proofs.push((public_key.clone(), at));
        state.next += 1;
        let device_code =
            format!("nestor_device_{}", &digest(&format!("{}-{}-device", state.next, state.now_ms))[..32]);
        let user_code = format!("DEMO-{:04}", state.next % 10_000);
        let approved = state.auto_approve;
        state.pending.push(PendingDevice {
            device_code_hash: digest(&device_code),
            user_code: user_code.clone(),
            public_key,
            name: name.into(),
            key_storage: key_storage.into(),
            expires_at_ms: state.now_ms + 600_000,
            approved,
            denied: false,
            consumed: false,
            last_proof_ms: at,
        });
        MachineResponse::json(
            201,
            json!({
                "device_code": device_code,
                "user_code": user_code,
                "verification_uri": format!("https://{}/machines/associate", self.instance),
                "expires_in": 600,
                "interval": 5,
            }),
        )
    }

    /// The request of association a poll or a cancellation names, once its proof is checked.
    fn device_request(
        &self,
        state: &mut FakeState,
        request: &MachineRequest,
        body: &Value,
    ) -> Result<usize, MachineResponse> {
        let Some(device_code) = body.get("device_code").and_then(Value::as_str) else {
            return Err(MachineResponse::error(422, "invalid_request"));
        };
        let hash = digest(device_code);
        let Some(index) = state.pending.iter().position(|p| p.device_code_hash == hash && !p.consumed) else {
            return Err(MachineResponse::error(401, "invalid_device_code"));
        };
        let (key, last) = (state.pending[index].public_key.clone(), state.pending[index].last_proof_ms);
        match self.check_proof(state, request, Some(device_code), &key, Some(last)) {
            ProofCheck::Valid(at) => {
                state.pending[index].last_proof_ms = at;
                Ok(index)
            }
            check => Err(Self::proof_refused(&check)),
        }
    }

    fn poll(&self, state: &mut FakeState, request: &MachineRequest, body: &Value) -> MachineResponse {
        let index = match self.device_request(state, request, body) {
            Ok(index) => index,
            Err(response) => return response,
        };
        let pending = state.pending[index].clone();
        if pending.expires_at_ms <= state.now_ms {
            return MachineResponse::error(400, "expired_token");
        }
        if pending.denied {
            return MachineResponse::error(400, "access_denied");
        }
        if !pending.approved {
            let mut response = MachineResponse::error(400, "authorization_pending");
            response.retry_after_s = Some(5);
            return response;
        }
        state.pending[index].consumed = true;
        state.next += 1;
        let machine_id = format!("00000000-0000-4000-8000-{:012x}", state.next);
        let credential = format!("nestor_machine_{}", &digest(&format!("{machine_id}-{}-secret", state.now_ms))[..32]);
        state.machines.insert(
            machine_id.clone(),
            FakeMachine {
                public_key: pending.public_key,
                key_storage: pending.key_storage,
                credential: credential.clone(),
                revoked: false,
                last_proof_ms: pending.last_proof_ms,
                runners: BTreeMap::new(),
                runner_credentials: Vec::new(),
                administrator_max: 1,
                paused_by_administrator: false,
                declared_max: None,
                declared_paused: false,
                last_heartbeat_ms: None,
                heartbeats: Vec::new(),
            },
        );
        MachineResponse::json(
            200,
            json!({
                "machine_id": machine_id,
                "name": pending.name,
                "credential": credential,
                "account_email": ACCOUNT_EMAIL,
                "approved_by_email": APPROVED_BY_EMAIL,
            }),
        )
    }

    fn cancel(&self, state: &mut FakeState, request: &MachineRequest, body: &Value) -> MachineResponse {
        match self.device_request(state, request, body) {
            Ok(index) => {
                state.pending[index].denied = true;
                MachineResponse::empty(204)
            }
            Err(response) => response,
        }
    }

    fn machine_route(&self, state: &mut FakeState, id: &str, target: &str, body: &Value) -> MachineResponse {
        let now = state.now_ms;
        match target {
            "/machine/heartbeat" => {
                let number = |name: &str| body.get(name).and_then(Value::as_i64);
                if number("machine_protocol_version") != Some(PROTOCOL_VERSION as i64) {
                    return MachineResponse::error(409, "incompatible_version");
                }
                let mode = body.get("mode").and_then(Value::as_str);
                let max = number("max_concurrent_tasks");
                let service_mode = body.get("service_mode").and_then(Value::as_str);
                if !matches!(mode, Some("automatic" | "paused"))
                    || !max.is_some_and(|m| (1..=16).contains(&m))
                    || !number("config_revision").is_some_and(|r| r >= 0)
                    || !matches!(service_mode, Some("live" | "demo"))
                {
                    return MachineResponse::error(422, "invalid_request");
                }
                let machine = state.machines.get_mut(id).unwrap();
                if machine.last_heartbeat_ms.is_some_and(|last| now < last + 5_000) {
                    let mut response = MachineResponse::error(429, "too_many_heartbeats");
                    response.retry_after_s = Some(5);
                    return response;
                }
                machine.last_heartbeat_ms = Some(now);
                machine.declared_max = max.map(|m| m as u32);
                machine.declared_paused = mode == Some("paused");
                machine.heartbeats.push(body.clone());
                let machine = state.machines[id].clone();
                MachineResponse::json(
                    200,
                    json!({
                        "interval_seconds": 15,
                        "leases": Self::leases(state, &machine),
                        "capabilities": Self::capabilities(state),
                    }),
                )
            }
            "/machine/queue" => {
                if !body.get("tools").is_some_and(Value::is_array) {
                    return MachineResponse::error(422, "invalid_request");
                }
                let mut scopes: Vec<String> = Vec::new();
                for (scope, count) in &state.queue.eligible {
                    scopes.extend(std::iter::repeat_n(scope.clone(), *count as usize));
                }
                let capped = scopes.len() > 64;
                scopes.truncate(64);
                let machine = state.machines[id].clone();
                MachineResponse::json(
                    200,
                    json!({
                        "eligible": scopes.len(),
                        "capped": capped,
                        "scopes": scopes,
                        "leases": Self::leases(state, &machine),
                        "capabilities": Self::capabilities(state),
                    }),
                )
            }
            "/machine/runners" => {
                if body.as_object().is_none_or(|o| !o.is_empty()) {
                    return MachineResponse::error(422, "invalid_request");
                }
                if state.machines[id].runners.values().filter(|r| !r.revoked).count() >= 16 {
                    return MachineResponse::error(409, "too_many_runners");
                }
                state.next += 1;
                let runner_id = format!("00000000-0000-4000-9000-{:012x}", state.next);
                let token = Self::new_token(state, &runner_id);
                let machine = state.machines.get_mut(id).unwrap();
                machine.runners.insert(runner_id.clone(), FakeRunner { token: token.clone(), revoked: false });
                let name = format!("Machine #{}", machine.runners.len());
                MachineResponse::json(
                    201,
                    json!({ "runner_id": runner_id, "name": name, "token": token, "expires_at": "2026-01-01T00:05:00Z" }),
                )
            }
            "/machine/disassociate" => {
                state.machines.get_mut(id).unwrap().revoked = true;
                state.revocations.push(id.to_string());
                MachineResponse::empty(204)
            }
            target => {
                let parts: Vec<&str> = target.trim_start_matches('/').split('/').collect();
                let ["machine", "runners", runner_id, "token"] = parts.as_slice() else {
                    return MachineResponse::error(404, "not_found");
                };
                let runner_id = runner_id.to_string();
                match state.machines[id].runners.get(&runner_id) {
                    None => return MachineResponse::error(404, "not_found"),
                    Some(runner) if runner.revoked => return MachineResponse::error(409, "runner_revoked"),
                    Some(_) => {}
                }
                let token = Self::new_token(state, &runner_id);
                state.machines.get_mut(id).unwrap().runners.get_mut(&runner_id).unwrap().token = token.clone();
                MachineResponse::json(
                    200,
                    json!({ "runner_id": runner_id, "token": token, "expires_at": "2026-01-01T00:05:00Z" }),
                )
            }
        }
        .with_issued(state, id)
    }

    fn new_token(state: &mut FakeState, runner_id: &str) -> String {
        state.next += 1;
        format!("nestor_runner_{}", &digest(&format!("{runner_id}-{}-token", state.next))[..32])
    }

    /// Revokes a runner of `machine_id` on Nestor, as `POST /machine/runners/{id}/revoke` would.
    pub fn revoke_runner(&self, machine_id: &str, runner_id: &str) {
        if let Some(runner) =
            self.state.lock().unwrap().machines.get_mut(machine_id).and_then(|m| m.runners.get_mut(runner_id))
        {
            runner.revoked = true;
        }
    }
}

trait Issued {
    fn with_issued(self, state: &mut FakeState, machine_id: &str) -> Self;
}

impl Issued for MachineResponse {
    /// Keeps every runner token issued, for the tests.
    fn with_issued(self, state: &mut FakeState, machine_id: &str) -> Self {
        if self.status == 200 || self.status == 201 {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.body) {
                if let Some(token) = value.get("token").and_then(Value::as_str) {
                    if let Some(machine) = state.machines.get_mut(machine_id) {
                        machine.runner_credentials.push(token.to_string());
                    }
                }
            }
        }
        self
    }
}

impl NestorApi for FakeNestor {
    fn instance(&self) -> String {
        self.instance.clone()
    }

    fn send(&self, request: &MachineRequest) -> Result<MachineResponse, String> {
        if self.state.lock().unwrap().unreachable {
            return Err("connection refused".into());
        }
        Ok(self.route(request))
    }
}
