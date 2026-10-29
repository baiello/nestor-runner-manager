//! The machine's association with Nestor (S9, B3, B6, B7).
//!
//! 1. **Key first.** `associate` generates the machine key (Secure Enclave in the live service)
//!    *before* contacting Nestor, and sends only its public part.
//! 2. **Device authorization** (RFC 8628). The service gets a secret device code (kept in memory,
//!    masked in every log) and a user code. The connection is `associating`: the interface shows
//!    the user code and the address of the Nestor page; an administrator approves it there. No
//!    secret is ever typed into or shown by the interface.
//! 3. **Polling** at the server's interval, each poll signed with the machine key. Approved: the
//!    machine credential goes to the [`SecretStore`] (Keychain), the connection is `associated`.
//!    Denied or expired: the key is erased.
//! 4. **Every call** to Nestor carries a proof of possession of the key. Runner credentials are
//!    issued per launch ([`CredentialIssuer`]) and never stored.
//! 5. **Disassociation** (local) and **revocation** (from Nestor): runners are terminated, the
//!    credential and the key are erased **even if Nestor cannot be reached**. A revocation
//!    request signed beforehand is kept (no secret in it) and retried until Nestor confirms.
//!
//! Lock order: the supervisor may call [`CredentialIssuer::issue`] while holding its own lock, so
//! this type never holds its own lock while calling the supervisor.

use std::sync::{Arc, Mutex};

use crate::mask::Redactor;
use crate::model::{Connection, MachineInfo, Queue, ServiceError};
use crate::nestor::{ApiError, DevicePoll, DeviceRequest, NestorApi, Proof};
use crate::persist::{RevocationRequest, RunnerRecord};
use crate::process::CredentialIssuer;
use crate::secrets::{MachineKey, MachineKeyStore, Secret, SecretStore, MACHINE_CREDENTIAL};
use crate::supervisor::{Clock, Supervisor};
use crate::sys::hex;

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
}

pub struct Associator {
    api: Option<Arc<dyn NestorApi>>,
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
        Associator { api, keys, secrets, supervisor, clock, redactor, state: Mutex::new(State::default()) }
    }

    pub fn key_protection(&self) -> crate::secrets::KeyProtection {
        self.keys.protection()
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn proof(&self, key: &dyn MachineKey, operation: &str, subject: &str) -> Result<Proof, String> {
        let message = Proof::message(operation, subject, self.now());
        let signature = key.sign(message.as_bytes())?;
        Ok(Proof { message, signature })
    }

    /// At startup: an association without its credential cannot work, and secrets without an
    /// association are leftovers to erase.
    pub fn startup(&self) {
        let connection = self.supervisor.connection();
        let credential = self.secrets.get(MACHINE_CREDENTIAL).ok().flatten();
        if connection.is_associated() {
            match (credential, self.keys.load()) {
                (Some(credential), Ok(Some(key))) => {
                    self.redactor.register(credential.expose());
                    self.state.lock().unwrap().key = Some(key);
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

    /// Starts a device authorization. The interface then shows the user code.
    pub fn associate(&self, machine: &MachineInfo) -> Result<Connection, ServiceError> {
        let connection = self.supervisor.connection();
        if matches!(connection, Connection::Associated { .. } | Connection::Associating { .. }) {
            return Ok(connection);
        }
        let api = self.api.clone().ok_or_else(|| {
            ServiceError::Unavailable(
                "Association with Nestor is not available in this version yet: it needs the Nestor API work (device authorization).".into(),
            )
        })?;
        // The key exists before anything is sent to Nestor.
        let key = self.keys.create().map_err(|e| ServiceError::Unavailable(self.redactor.mask(&e)))?;
        let public_key = key.public_key().map_err(ServiceError::Unavailable)?;
        let request = DeviceRequest {
            public_key,
            machine_name: machine.name.clone(),
            model: machine.model.clone(),
            operating_system: machine.operating_system.clone(),
        };
        let authorization = match api.start_device_authorization(&request) {
            Ok(authorization) => authorization,
            Err(error) => {
                let _ = self.keys.delete();
                return Err(api_error(error));
            }
        };
        self.redactor.register(authorization.device_code.expose());
        let now = self.now();
        let connection = Connection::Associating {
            instance: api.instance(),
            user_code: authorization.user_code.clone(),
            verification_uri: authorization.verification_uri.clone(),
            expires_at_ms: now + authorization.expires_in_ms,
        };
        self.state.lock().unwrap().pending = Some(Pending {
            instance: api.instance(),
            device_code: authorization.device_code,
            key,
            interval_ms: authorization.interval_ms.max(1_000),
            next_poll_ms: now + authorization.interval_ms.max(1_000),
            expires_at_ms: now + authorization.expires_in_ms,
        });
        self.supervisor.set_connection(connection.clone());
        Ok(connection)
    }

    /// Abandons an association in progress.
    pub fn cancel(&self) -> Connection {
        let pending = self.state.lock().unwrap().pending.take();
        if pending.is_some() {
            let _ = self.keys.delete();
            self.supervisor.set_connection(Connection::NotAssociated);
        }
        self.supervisor.connection()
    }

    /// Polls the association in progress, retries a pending revocation, and fetches the queue
    /// when associated. Returns what the supervisor should regulate on.
    pub fn tick(&self) -> Queue {
        self.retry_revocation();
        self.poll();
        self.fetch_queue()
    }

    fn poll(&self) {
        let Some(api) = &self.api else { return };
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
        let proof = match self.proof(key.as_ref(), "poll", "device") {
            Ok(proof) => proof,
            Err(error) => return self.supervisor.report_error(&error),
        };
        match api.poll_device_authorization(&device_code, &proof) {
            Ok(DevicePoll::Pending) | Err(ApiError::Unreachable(_)) => {}
            Ok(DevicePoll::SlowDown) => {
                if let Some(pending) = self.state.lock().unwrap().pending.as_mut() {
                    pending.interval_ms += 5_000;
                }
            }
            Ok(DevicePoll::Approved { machine_id, credential }) => {
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
                self.supervisor.set_connection(Connection::Associated { instance, since_ms: now });
            }
            Ok(DevicePoll::Denied) => {
                self.state.lock().unwrap().pending = None;
                self.failed("The association was refused on Nestor.");
            }
            Ok(DevicePoll::Expired) | Err(ApiError::Revoked) => {
                self.state.lock().unwrap().pending = None;
                self.failed("The association code expired. Associate again.");
            }
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

    fn fetch_queue(&self) -> Queue {
        let Some(api) = &self.api else { return Queue::empty() };
        let connection = self.supervisor.connection();
        let instance = match &connection {
            Connection::Associated { instance, .. } | Connection::Unreachable { instance, .. } => instance.clone(),
            _ => return Queue::empty(),
        };
        let Some(key) = self.state.lock().unwrap().key.clone() else { return Queue::empty() };
        let Ok(Some(credential)) = self.secrets.get(MACHINE_CREDENTIAL) else {
            self.supervisor.report_error("The machine credential is missing: associate again.");
            self.erase_secrets();
            self.supervisor.end_association(Connection::NotAssociated);
            return Queue::empty();
        };
        let proof = match self.proof(key.as_ref(), "queue", "machine") {
            Ok(proof) => proof,
            Err(error) => {
                self.supervisor.report_error(&error);
                return Queue::empty();
            }
        };
        match api.queue(&credential, &proof) {
            Ok(queue) => {
                if let Connection::Unreachable { .. } = connection {
                    self.supervisor.set_connection(Connection::Associated { instance, since_ms: self.now() });
                }
                queue
            }
            Err(ApiError::Unreachable(reason)) => {
                self.supervisor.set_connection(Connection::Unreachable { instance, reason });
                Queue::empty()
            }
            Err(ApiError::Revoked) => {
                self.revoked();
                Queue::empty()
            }
            Err(ApiError::Rejected(reason)) => {
                self.supervisor.report_error(&format!("Nestor refused the request: {reason}"));
                Queue::empty()
            }
        }
    }

    /// Nestor revoked the machine: terminate runners, erase everything.
    fn revoked(&self) {
        self.erase_secrets();
        self.supervisor.end_association(Connection::Revoked);
    }

    /// Ends the association from this machine. The interface confirms first: running tasks
    /// end. Works offline: secrets are erased at once, the revocation is retried later.
    pub fn disassociate(&self) -> Connection {
        let connection = self.supervisor.connection();
        if matches!(connection, Connection::Associating { .. }) {
            return self.cancel();
        }
        let key = self.state.lock().unwrap().key.clone();
        if let (
            Some(key),
            Some(machine_id),
            Connection::Associated { instance, .. } | Connection::Unreachable { instance, .. },
        ) = (key, self.supervisor.machine_id(), &connection)
        {
            let issued_at_ms = self.now();
            let message = RevocationRequest::message(instance, &machine_id, issued_at_ms);
            match key.sign(message.as_bytes()) {
                Ok(signature) => self.supervisor.set_pending_revocation(Some(RevocationRequest {
                    instance: instance.clone(),
                    machine_id,
                    issued_at_ms,
                    signature: hex(&signature),
                })),
                Err(error) => self.supervisor.report_error(&format!("Could not sign the revocation: {error}")),
            }
        }
        self.erase_secrets();
        self.supervisor.end_association(Connection::NotAssociated);
        self.retry_revocation();
        self.supervisor.connection()
    }

    fn retry_revocation(&self) {
        let (Some(api), Some(request)) = (&self.api, self.supervisor.pending_revocation()) else { return };
        if request.instance != api.instance() {
            return;
        }
        match api.revoke(&request) {
            Ok(()) => self.supervisor.set_pending_revocation(None),
            Err(ApiError::Unreachable(_)) => {}
            Err(error) => {
                self.supervisor.report_error(&format!("Nestor refused the revocation: {error:?}"));
                self.supervisor.set_pending_revocation(None);
            }
        }
    }

    fn erase_secrets(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.key = None;
            state.pending = None;
        }
        if let Err(error) = self.secrets.delete(MACHINE_CREDENTIAL) {
            self.supervisor.report_error(&format!("Could not erase the machine credential: {error}"));
        }
        if let Err(error) = self.keys.delete() {
            self.supervisor.report_error(&format!("Could not erase the machine key: {error}"));
        }
    }
}

impl CredentialIssuer for Associator {
    fn issue(&self, runner: &RunnerRecord) -> Result<Secret, String> {
        let api = self.api.as_ref().ok_or("no Nestor API")?;
        let key = self.state.lock().unwrap().key.clone().ok_or("not associated")?;
        let credential = self.secrets.get(MACHINE_CREDENTIAL)?.ok_or("no machine credential")?;
        let proof = self.proof(key.as_ref(), "runner-credential", &runner.id)?;
        api.issue_runner_credential(&credential, &proof, &runner.id).map_err(|e| format!("{e:?}"))
    }
}

fn api_error(error: ApiError) -> ServiceError {
    match error {
        ApiError::Unreachable(_) => ServiceError::Unreachable,
        ApiError::Revoked => ServiceError::Revoked,
        ApiError::Rejected(reason) => ServiceError::Unavailable(format!("Nestor refused: {reason}")),
    }
}
