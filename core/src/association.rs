//! The machine's association with Nestor (S9, B3, B6, B7, M1 and M2 of KCA-738).
//!
//! 1. **Key first.** `associate` generates the machine key (Secure Enclave in the live service)
//!    *before* contacting Nestor, and sends only its public part.
//! 2. **Device authorization** (RFC 8628). The service gets a secret device code (kept in memory,
//!    masked in every log) and a user code. The connection is `associating`: the interface shows
//!    the user code, the address of the Nestor page, and that the code must never be given to
//!    anyone; an administrator approves it there. No secret is ever typed into or shown by the
//!    interface.
//! 3. **Polling** at the server's interval, each poll signed with the machine key. Approved: the
//!    machine credential goes to the [`SecretStore`] (Keychain), and the connection is
//!    `confirming`: the interface shows the account the machine would work for and who
//!    approved it, both as Nestor masks them, and **no runner is created** until the owner
//!    confirms them on this machine (M1). Refused there, the machine disassociates itself.
//!    Denied or expired on Nestor: the key is erased.
//! 4. **Every request** to Nestor carries a proof `nestor-machine-proof/v2` ([`MachineClient`]),
//!    sent one at a time, each signed after the last. Associated, the service sends its
//!    heartbeat every interval the server gives, then reads the queue, and applies the leases
//!    and capabilities both answers carry. Runner tokens are issued per launch
//!    ([`CredentialIssuer`]) for a runner of the machine on Nestor, and never stored.
//! 5. **Disassociation** (local) and **revocation** (from Nestor): runners are terminated, the
//!    credential and the key are erased **even if Nestor cannot be reached**. The service tells
//!    Nestor first (`POST /machine/disassociate`); when it cannot, it says so, and the machine
//!    must be revoked on the Nestor web app: a proof is good 60 seconds, so no revocation can
//!    be signed now and sent later.
//!
//! Lock order: the supervisor may call [`CredentialIssuer::issue`] while holding its own lock, so
//! this type never holds its own lock while calling the supervisor.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::mask::Redactor;
use crate::model::{Connection, MachineInfo, MemoryPressure, Queue, ServiceError};
use crate::nestor::{
    ApiError, Capabilities, DevicePoll, DeviceRequest, HeartbeatReport, Leases, MachineClient, NestorApi,
};
use crate::persist::RunnerRecord;
use crate::process::CredentialIssuer;
use crate::secrets::{KeyProtection, MachineKey, MachineKeyStore, Secret, SecretStore, MACHINE_CREDENTIAL};
use crate::supervisor::{Clock, ServerTerms, Supervisor};

/// Where the runners of the machine on Nestor are kept, by local runner: not a secret, kept with
/// the secrets so that it goes with them.
pub const NESTOR_RUNNERS: &str = "nestor-runners";
/// Before the server gives its own.
const HEARTBEAT_INTERVAL_MS: u64 = 15_000;

struct Pending {
    instance: String,
    device_code: Secret,
    key: Arc<dyn MachineKey>,
    interval_ms: u64,
    next_poll_ms: u64,
    expires_at_ms: u64,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    key: Option<Arc<dyn MachineKey>>,
    /// The owner confirmed the association (M1): runners may be created. Kept here, not read
    /// from the supervisor, which calls [`CredentialIssuer::issue`] under its lock.
    confirmed: bool,
    next_heartbeat_ms: u64,
}

pub struct Associator {
    client: Option<MachineClient>,
    keys: Arc<dyn MachineKeyStore>,
    secrets: Arc<dyn SecretStore>,
    supervisor: Arc<Supervisor>,
    clock: Clock,
    redactor: Redactor,
    state: Mutex<State>,
}

impl Associator {
    pub fn new(
        api: Option<Arc<dyn NestorApi>>,
        keys: Arc<dyn MachineKeyStore>,
        secrets: Arc<dyn SecretStore>,
        supervisor: Arc<Supervisor>,
        clock: Clock,
    ) -> Self {
        let redactor = supervisor.redactor().clone();
        let client = api.map(|api| MachineClient::new(api, clock.clone()));
        Associator { client, keys, secrets, supervisor, clock, redactor, state: Mutex::new(State::default()) }
    }

    pub fn key_protection(&self) -> KeyProtection {
        self.keys.protection()
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// At startup: an association without its credential cannot work, and secrets without an
    /// association are leftovers to erase. A revocation signed by an earlier release cannot be
    /// sent any more (its proof is long stale): the owner is told to revoke on Nestor.
    pub fn startup(&self) {
        if let Some(leftover) = self.supervisor.pending_revocation() {
            self.supervisor.set_pending_revocation(None);
            self.supervisor.report_error(&format!(
                "This machine was disassociated while Nestor could not be reached: revoke it on the Nestor web app (Machines) if it is still listed there ({}).",
                leftover.instance
            ));
        }
        let connection = self.supervisor.connection();
        let credential = self.secrets.get(MACHINE_CREDENTIAL).ok().flatten();
        if connection.is_associated() || matches!(connection, Connection::Confirming { .. }) {
            match (credential, self.keys.load()) {
                (Some(credential), Ok(Some(key))) => {
                    self.redactor.register(credential.expose());
                    let mut state = self.state.lock().unwrap();
                    state.key = Some(key);
                    state.confirmed = connection.is_associated();
                }
                _ => {
                    self.supervisor.report_error("The machine credential or key is missing: associate again.");
                    self.erase_secrets();
                    self.supervisor.end_association(Connection::NotAssociated);
                }
            }
        } else if credential.is_some() || matches!(self.keys.load(), Ok(Some(_))) {
            self.erase_secrets();
        }
    }

    fn device_request(&self, machine: &MachineInfo, public_key: Vec<u8>) -> DeviceRequest {
        let (os, os_version) = match machine.operating_system.split_once(' ') {
            Some((name, version)) => (name, version),
            None => (machine.operating_system.as_str(), ""),
        };
        let os = match os.to_ascii_lowercase().as_str() {
            "macos" => "macos",
            "linux" => "linux",
            "windows" => "windows",
            _ => "other",
        };
        let arch = match machine.architecture.as_str() {
            "arm64" | "aarch64" => "aarch64",
            "x86_64" | "amd64" => "x86_64",
            _ => "other",
        };
        DeviceRequest {
            public_key,
            machine_name: machine.name.clone(),
            model: machine.model.clone(),
            os: os.into(),
            os_version: os_version.into(),
            arch: arch.into(),
            // What the key really is: nestor accepts the Secure Enclave only (A6).
            key_storage: match self.keys.protection() {
                KeyProtection::SecureEnclave => "secure_enclave",
                KeyProtection::Software | KeyProtection::Memory => "software",
            }
            .into(),
        }
    }

    /// Starts a device authorization. The interface then shows the user code.
    pub fn associate(&self, machine: &MachineInfo) -> Result<Connection, ServiceError> {
        let connection = self.supervisor.connection();
        if matches!(
            connection,
            Connection::Associated { .. } | Connection::Associating { .. } | Connection::Confirming { .. }
        ) {
            return Ok(connection);
        }
        let client = self.client.as_ref().ok_or_else(|| {
            ServiceError::Unavailable(
                "Association with Nestor is not available in this version yet: it needs the Nestor API work (device authorization).".into(),
            )
        })?;
        // The key exists before anything is sent to Nestor.
        let key = self.keys.create().map_err(|e| ServiceError::Unavailable(self.redactor.mask(&e)))?;
        let public_key = key.public_key().map_err(ServiceError::Unavailable)?;
        let request = self.device_request(machine, public_key);
        let authorization = match client.request_authorization(key.as_ref(), &request) {
            Ok(authorization) => authorization,
            Err(error) => {
                let _ = self.keys.delete();
                return Err(api_error(error));
            }
        };
        self.redactor.register(authorization.device_code.expose());
        let now = self.now();
        let instance = client.instance();
        let connection = Connection::Associating {
            instance: instance.clone(),
            user_code: authorization.user_code.clone(),
            verification_uri: authorization.verification_uri.clone(),
            expires_at_ms: now + authorization.expires_in_ms,
        };
        let interval_ms = authorization.interval_ms.max(1_000);
        self.state.lock().unwrap().pending = Some(Pending {
            instance,
            device_code: authorization.device_code,
            key,
            interval_ms,
            next_poll_ms: now + interval_ms,
            expires_at_ms: now + authorization.expires_in_ms,
        });
        self.supervisor.set_connection(connection.clone());
        Ok(connection)
    }

    /// Abandons an association in progress, and tells Nestor so if it can.
    pub fn cancel(&self) -> Connection {
        let pending = self.state.lock().unwrap().pending.take();
        if let Some(pending) = pending {
            if let Some(client) = &self.client {
                let _ = client.cancel(pending.key.as_ref(), &pending.device_code);
            }
            let _ = self.keys.delete();
            self.supervisor.set_connection(Connection::NotAssociated);
        }
        self.supervisor.connection()
    }

    /// The owner's answer, on this machine, to the account and approver Nestor gave (M1).
    /// Confirmed, the machine is associated and may create runners; refused, it disassociates
    /// itself and erases its secrets.
    pub fn confirm(&self, confirmed: bool) -> Result<Connection, ServiceError> {
        let Connection::Confirming { instance, .. } = self.supervisor.connection() else {
            return Err(ServiceError::Unavailable("No association waits for a confirmation.".into()));
        };
        if !confirmed {
            return Ok(self.disassociate());
        }
        {
            let mut state = self.state.lock().unwrap();
            state.next_heartbeat_ms = 0;
            state.confirmed = true;
        }
        self.supervisor.set_connection(Connection::Associated { instance, since_ms: self.now() });
        Ok(self.supervisor.connection())
    }

    /// Polls the association in progress; associated, sends the heartbeat when it is due and
    /// reads the queue. Returns what the supervisor should regulate on.
    pub fn tick(&self) -> Queue {
        self.poll();
        if !self.supervisor.connection().is_associated() {
            return Queue::empty();
        }
        let Some((client, key, credential)) = self.machine() else { return Queue::empty() };
        if self.now() >= self.state.lock().unwrap().next_heartbeat_ms && !self.heartbeat(client, &key, &credential) {
            return Queue::empty();
        }
        self.fetch_queue(client, &key, &credential)
    }

    /// The client, key and credential of the association, or why there are none (handled).
    fn machine(&self) -> Option<(&MachineClient, Arc<dyn MachineKey>, Secret)> {
        let client = self.client.as_ref()?;
        let key = self.state.lock().unwrap().key.clone()?;
        match self.secrets.get(MACHINE_CREDENTIAL) {
            Ok(Some(credential)) => Some((client, key, credential)),
            _ => {
                self.supervisor.report_error("The machine credential is missing: associate again.");
                self.erase_secrets();
                self.supervisor.end_association(Connection::NotAssociated);
                None
            }
        }
    }

    fn poll(&self) {
        let Some(client) = &self.client else { return };
        let now = self.now();
        let (device_code, key, instance) = {
            let mut state = self.state.lock().unwrap();
            let Some(pending) = state.pending.as_mut() else { return };
            if now >= pending.expires_at_ms {
                state.pending = None;
                drop(state);
                self.failed("The association code expired. Associate again.");
                return;
            }
            if now < pending.next_poll_ms {
                return;
            }
            pending.next_poll_ms = now + pending.interval_ms;
            (pending.device_code.clone(), pending.key.clone(), pending.instance.clone())
        };
        match client.poll(key.as_ref(), &device_code) {
            Ok(DevicePoll::Pending) | Err(ApiError::Unreachable(_)) => {}
            Ok(DevicePoll::SlowDown { interval_ms }) => {
                if let Some(pending) = self.state.lock().unwrap().pending.as_mut() {
                    pending.interval_ms = interval_ms.unwrap_or(pending.interval_ms + 5_000).min(60_000);
                    pending.next_poll_ms = now + pending.interval_ms;
                }
            }
            Ok(DevicePoll::Held(reason)) => self.supervisor.report_error(&match reason.as_str() {
                "too_many_machines" => "Nestor approved this machine, but the account has too many machines: revoke one on Nestor, the association then goes on.".to_string(),
                _ => "Nestor approved this machine, but its key is still used by another machine: revoke that one on Nestor, the association then goes on.".to_string(),
            }),
            Ok(DevicePoll::Approved { machine_id, credential, account_email, approved_by_email }) => {
                self.redactor.register(credential.expose());
                if let Err(error) = self.secrets.put(MACHINE_CREDENTIAL, &credential) {
                    self.state.lock().unwrap().pending = None;
                    self.failed(&format!("Could not store the machine credential: {error}"));
                    return;
                }
                {
                    let mut state = self.state.lock().unwrap();
                    state.pending = None;
                    state.key = Some(key);
                }
                self.supervisor.set_machine_id(Some(machine_id));
                self.supervisor.set_connection(Connection::Confirming { instance, account_email, approved_by_email });
            }
            Ok(DevicePoll::Denied) => {
                self.state.lock().unwrap().pending = None;
                self.failed("The association was refused on Nestor.");
            }
            Ok(DevicePoll::Expired) | Err(ApiError::Revoked) => {
                self.state.lock().unwrap().pending = None;
                self.failed("The association code expired. Associate again.");
            }
            Err(ApiError::StaleProof) => self.supervisor.report_error(CLOCK_OFF),
            Err(ApiError::Rejected(reason)) => {
                self.state.lock().unwrap().pending = None;
                self.failed(&format!("Nestor refused the association: {reason}"));
            }
        }
    }

    fn failed(&self, message: &str) {
        let _ = self.keys.delete();
        self.supervisor.report_error(message);
        self.supervisor.set_connection(Connection::NotAssociated);
    }

    fn report(&self) -> HeartbeatReport {
        let status = self.supervisor.status();
        let load = status.load.as_ref();
        HeartbeatReport {
            service_mode: status.service_mode,
            mode: status.mode,
            // What the service allows itself: the ceiling it applies.
            max_concurrent_tasks: status.effective_max_concurrent_tasks,
            config_revision: status.config_revision,
            cpu_percent: load.map(|l| l.measured_cpu_percent),
            memory_percent: load
                .filter(|l| l.measured_memory_total_bytes > 0)
                .map(|l| l.measured_memory_used_bytes as f64 * 100.0 / l.measured_memory_total_bytes as f64),
            memory_pressure: load.and_then(|l| l.measured_memory_pressure).map(|p| match p {
                MemoryPressure::Normal => "normal",
                MemoryPressure::Warning => "warning",
                MemoryPressure::Critical => "critical",
            }),
            disk_free_bytes: load.map(|l| l.measured_disk_free_bytes),
        }
    }

    /// `POST /machine/heartbeat`. Returns whether the queue should be read now.
    fn heartbeat(&self, client: &MachineClient, key: &Arc<dyn MachineKey>, credential: &Secret) -> bool {
        let report = self.report();
        let now = self.now();
        match client.heartbeat(key.as_ref(), credential, &report) {
            Ok(answer) => {
                self.state.lock().unwrap().next_heartbeat_ms =
                    now + answer.interval_seconds.clamp(5, 300).saturating_mul(1_000);
                self.reached(client);
                self.apply(&answer.leases, &answer.capabilities);
                true
            }
            Err(ApiError::Rejected(code)) if code == "too_many_heartbeats" => {
                self.state.lock().unwrap().next_heartbeat_ms = now + 5_000;
                true
            }
            Err(error) => {
                self.state.lock().unwrap().next_heartbeat_ms = now + HEARTBEAT_INTERVAL_MS;
                self.refused(client, error);
                false
            }
        }
    }

    /// `POST /machine/queue`: what a new runner may claim, per scope.
    fn fetch_queue(&self, client: &MachineClient, key: &Arc<dyn MachineKey>, credential: &Secret) -> Queue {
        match client.queue(key.as_ref(), credential) {
            Ok(answer) => {
                self.reached(client);
                self.apply(&answer.leases, &answer.capabilities);
                answer.queue()
            }
            Err(error) => {
                self.refused(client, error);
                Queue::empty()
            }
        }
    }

    fn apply(&self, leases: &Leases, capabilities: &Capabilities) {
        self.supervisor.set_server_terms(ServerTerms {
            scope_filter: capabilities.scope_filter,
            release: capabilities.release,
            administrator_max: Some(leases.administrator_max),
            paused_by_administrator: leases.paused_by_administrator,
        });
    }

    fn reached(&self, client: &MachineClient) {
        if let Connection::Unreachable { instance, .. } = self.supervisor.connection() {
            if instance == client.instance() {
                self.supervisor.set_connection(Connection::Associated { instance, since_ms: self.now() });
            }
        }
    }

    fn refused(&self, client: &MachineClient, error: ApiError) {
        match error {
            ApiError::Unreachable(reason) => {
                self.supervisor.set_connection(Connection::Unreachable { instance: client.instance(), reason })
            }
            ApiError::Revoked => self.revoked(),
            ApiError::StaleProof => self.supervisor.report_error(CLOCK_OFF),
            ApiError::Rejected(reason) => {
                self.supervisor.report_error(&format!("Nestor refused the request: {reason}"))
            }
        }
    }

    /// Nestor revoked the machine: terminate runners, erase everything.
    fn revoked(&self) {
        self.erase_secrets();
        self.supervisor.end_association(Connection::Revoked);
    }

    /// Ends the association from this machine. The interface confirms first: running tasks
    /// end. Works offline: secrets are erased at once; Nestor is told first when it can be.
    pub fn disassociate(&self) -> Connection {
        let connection = self.supervisor.connection();
        if matches!(connection, Connection::Associating { .. }) {
            return self.cancel();
        }
        let key = self.state.lock().unwrap().key.clone();
        let credential = self.secrets.get(MACHINE_CREDENTIAL).ok().flatten();
        if let (Some(client), Some(key), Some(credential)) = (&self.client, key, credential) {
            match client.disassociate(key.as_ref(), &credential) {
                Ok(()) | Err(ApiError::Revoked) => {}
                Err(error) => self.supervisor.report_error(&format!(
                    "Nestor could not be told ({}): revoke this machine on the Nestor web app (Machines).",
                    match error {
                        ApiError::Unreachable(_) => "it cannot be reached".to_string(),
                        ApiError::StaleProof => "this machine's clock is off".to_string(),
                        ApiError::Rejected(reason) => reason,
                        ApiError::Revoked => unreachable!(),
                    }
                )),
            }
        }
        self.erase_secrets();
        self.supervisor.end_association(Connection::NotAssociated);
        self.supervisor.connection()
    }

    fn erase_secrets(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.key = None;
            state.pending = None;
            state.confirmed = false;
            state.next_heartbeat_ms = 0;
        }
        for name in [MACHINE_CREDENTIAL, NESTOR_RUNNERS] {
            if let Err(error) = self.secrets.delete(name) {
                self.supervisor.report_error(&format!("Could not erase the machine credential: {error}"));
            }
        }
        if let Err(error) = self.keys.delete() {
            self.supervisor.report_error(&format!("Could not erase the machine key: {error}"));
        }
    }

    /// The runner of the machine on Nestor of each local runner.
    fn nestor_runners(&self) -> BTreeMap<String, String> {
        let stored = self.secrets.get(NESTOR_RUNNERS).ok().flatten();
        stored.and_then(|s| serde_json::from_str(s.expose()).ok()).unwrap_or_default()
    }

    fn keep_nestor_runner(&self, local: &str, nestor: &str) -> Result<(), String> {
        let mut runners = self.nestor_runners();
        runners.insert(local.into(), nestor.into());
        self.secrets.put(NESTOR_RUNNERS, &Secret::new(serde_json::to_string(&runners).unwrap()))
    }
}

const CLOCK_OFF: &str =
    "Nestor refused the time of this machine's request again: set this machine's clock (System Settings › General › Date & Time).";

impl CredentialIssuer for Associator {
    /// A token for one claim, for the runner of the machine on Nestor this local runner stands
    /// for: created the first time, given a new token afterwards. Only once associated and
    /// confirmed (M1).
    fn issue(&self, runner: &RunnerRecord) -> Result<Secret, String> {
        let client = self.client.as_ref().ok_or("no Nestor API")?;
        let (key, confirmed) = {
            let state = self.state.lock().unwrap();
            (state.key.clone().ok_or("not associated")?, state.confirmed)
        };
        if !confirmed {
            return Err("the association is not confirmed on this machine".into());
        }
        let credential = self.secrets.get(MACHINE_CREDENTIAL)?.ok_or("no machine credential")?;
        if let Some(nestor) = self.nestor_runners().get(&runner.id) {
            match client.runner_token(key.as_ref(), &credential, nestor) {
                Ok(token) => return Ok(token),
                // Gone on Nestor: a new runner of the machine stands for it.
                Err(ApiError::Rejected(code)) if code == "not_found" || code == "runner_revoked" => {}
                Err(error) => return Err(format!("{error:?}")),
            }
        }
        let issued = client.create_runner(key.as_ref(), &credential).map_err(|e| format!("{e:?}"))?;
        self.keep_nestor_runner(&runner.id, &issued.runner_id)?;
        Ok(issued.token)
    }
}

fn api_error(error: ApiError) -> ServiceError {
    match error {
        ApiError::Unreachable(_) => ServiceError::Unreachable,
        ApiError::Revoked => ServiceError::Revoked,
        ApiError::StaleProof => ServiceError::Unavailable(CLOCK_OFF.into()),
        ApiError::Rejected(code) if code == "key_storage_not_accepted" => ServiceError::Unavailable(
            "Nestor accepts only a key kept in the Secure Enclave, which this build cannot create.".into(),
        ),
        ApiError::Rejected(reason) => ServiceError::Unavailable(format!("Nestor refused: {reason}")),
    }
}
