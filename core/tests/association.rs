//! Association against a fake Nestor (S9, B3, B6, B7, S8.5, N8, A6).

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::{queue, Harness};
use nestor_runner_core::association::Associator;
use nestor_runner_core::model::*;
use nestor_runner_core::nestor::{FakeNestor, NestorApi};
use nestor_runner_core::persist::RunnerRecord;
use nestor_runner_core::process::CredentialIssuer;
use nestor_runner_core::secrets::*;
use nestor_runner_core::supervisor::Clock;

struct Setup {
    harness: Harness,
    nestor: Arc<FakeNestor>,
    secrets: Arc<MemorySecretStore>,
    keys: Arc<MemoryKeyStore>,
    associator: Arc<Associator>,
}

fn machine() -> MachineInfo {
    MachineInfo {
        name: "Studio".into(),
        model: "Mac17,14".into(),
        architecture: "arm64".into(),
        operating_system: "macOS 27".into(),
        cpu_cores: 18,
        memory_bytes: 1,
        disk_free_bytes: 1,
    }
}

fn setup() -> Setup {
    let harness = Harness::new();
    harness.supervisor.reconcile();
    let nestor = Arc::new(FakeNestor::new("nestor.test"));
    nestor.set_now(harness.time.load(Ordering::SeqCst));
    // A server with the scope filter of S6, so the requested ceiling applies.
    nestor.set_scope_filter(true);
    let secrets = Arc::new(MemorySecretStore::new());
    let keys = Arc::new(MemoryKeyStore::new());
    let time = harness.time.clone();
    let clock: Clock = Arc::new(move || time.load(Ordering::SeqCst));
    let associator = Arc::new(Associator::new(
        Some(nestor.clone() as Arc<dyn NestorApi>),
        keys.clone(),
        secrets.clone(),
        harness.supervisor.clone(),
        clock,
    ));
    Setup { harness, nestor, secrets, keys, associator }
}

impl Setup {
    /// One second of service life: the association polls, the supervisor regulates.
    fn tick(&self, ms: u64) {
        self.harness.advance(ms);
        self.nestor.set_now(self.harness.time.load(Ordering::SeqCst));
        let queue = self.associator.tick();
        self.harness.supervisor.tick(&queue);
    }

    fn associate(&self) -> String {
        let connection = self.associator.associate(&machine()).unwrap();
        let Connection::Associating { user_code, .. } = connection else { panic!("{connection:?}") };
        assert!(self.nestor.approve(&user_code));
        self.tick(5_000);
        assert!(matches!(self.harness.supervisor.connection(), Connection::Associated { .. }));
        self.harness.supervisor.machine_id().unwrap()
    }

    fn everything_visible(&self) -> String {
        let supervisor = &self.harness.supervisor;
        let mut text = serde_json::to_string(&supervisor.status()).unwrap();
        text += &serde_json::to_string(&supervisor.events_since(0).unwrap()).unwrap();
        for runner in supervisor.runners() {
            text += &serde_json::to_string(&supervisor.logs(&runner.id, 500).unwrap()).unwrap();
        }
        let stored = self.harness.store.load();
        text += &serde_json::to_string(stored.state()).unwrap();
        text
    }
}

#[test]
fn the_key_exists_before_the_request_and_the_flow_needs_an_approval_on_the_web() {
    let setup = setup();
    assert!(setup.keys.load().unwrap().is_none());
    let connection = setup.associator.associate(&machine()).unwrap();
    let Connection::Associating { user_code, verification_uri, instance, .. } = &connection else { panic!() };
    assert_eq!(instance, "nestor.test");
    assert!(verification_uri.starts_with("https://nestor.test/"));
    let key = setup.keys.load().unwrap().expect("generated before the request");
    // Nothing without the approval: polls stay pending, nothing is launched.
    for _ in 0..3 {
        setup.tick(5_000);
    }
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Associating { .. }));
    assert_eq!(setup.harness.launcher.spawns(), 0);
    assert!(setup.secrets.get(MACHINE_CREDENTIAL).unwrap().is_none());

    assert!(setup.nestor.approve(user_code));
    setup.tick(5_000);
    let machine_id = setup.harness.supervisor.machine_id().unwrap();
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Associated { .. }));
    assert_eq!(setup.nestor.public_key_of(&machine_id).unwrap(), key.public_key().unwrap(), "bound to the key");
    assert!(setup.secrets.get(MACHINE_CREDENTIAL).unwrap().is_some());
    assert_eq!(setup.nestor.rejected_proofs(), 0, "every call carried a valid proof of possession");
    // The device code and the credential appear nowhere.
    let credential = setup.secrets.get(MACHINE_CREDENTIAL).unwrap().unwrap();
    let text = setup.everything_visible();
    assert!(!text.contains("nestor_device_") && !text.contains(credential.expose()), "{text}");
    // The user code is single use: approving it again does nothing.
    assert!(!setup.nestor.approve(user_code));
}

#[test]
fn denied_or_expired_associations_erase_the_key() {
    let setup = setup();
    let Connection::Associating { user_code, .. } = setup.associator.associate(&machine()).unwrap() else { panic!() };
    setup.nestor.deny(&user_code);
    setup.tick(5_000);
    assert_eq!(setup.harness.supervisor.connection(), Connection::NotAssociated);
    assert!(setup.keys.load().unwrap().is_none());

    setup.associator.associate(&machine()).unwrap();
    setup.tick(600_000);
    assert_eq!(setup.harness.supervisor.connection(), Connection::NotAssociated, "expired after 10 minutes");
    assert!(setup.keys.load().unwrap().is_none());

    setup.associator.associate(&machine()).unwrap();
    assert_eq!(setup.associator.cancel(), Connection::NotAssociated);
    assert!(setup.keys.load().unwrap().is_none());
}

#[test]
fn an_unreachable_nestor_blocks_launches_and_recovers() {
    let setup = setup();
    setup.associate();
    setup.nestor.set_queue(queue(2));
    setup.tick(1_000);
    assert_eq!(setup.harness.launcher.spawns(), 1);
    setup.nestor.set_unreachable(true);
    setup.tick(1_000);
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Unreachable { .. }));
    setup.nestor.set_unreachable(false);
    setup.tick(1_000);
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Associated { .. }));
}

#[test]
fn runner_credentials_are_issued_per_launch_with_a_proof() {
    let setup = setup();
    let machine_id = setup.associate();
    let record = RunnerRecord {
        id: "0123456789abcdef0123456789abcdef".into(),
        name: "runner-1".into(),
        generation: 1,
        pid: None,
        start_time_us: None,
        launching_since: None,
        scope: None,
        created_at_ms: 0,
        manual: false,
    };
    let first = setup.associator.issue(&record).unwrap();
    let second = setup.associator.issue(&record).unwrap();
    assert_ne!(first, second, "a new credential for each launch");
    assert_eq!(setup.nestor.runner_credentials(&machine_id).len(), 2);
    assert_eq!(setup.nestor.rejected_proofs(), 0);
}

#[test]
fn disassociating_offline_erases_secrets_at_once_and_retries_the_revocation() {
    let setup = setup();
    let machine_id = setup.associate();
    setup.harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    setup.nestor.set_queue(queue(2));
    setup.tick(1_000);
    setup.harness.bring_up();
    assert_eq!(setup.harness.claim_all().len(), 2);

    setup.nestor.set_unreachable(true);
    let connection = setup.associator.disassociate();
    assert_eq!(connection, Connection::NotAssociated);
    // Secrets erased even though Nestor cannot be reached.
    assert!(setup.secrets.get(MACHINE_CREDENTIAL).unwrap().is_none());
    assert!(setup.keys.load().unwrap().is_none());
    // Runners terminated: disassociation is an exception to "never interrupt".
    assert_eq!(setup.harness.launcher.terminated().len(), 2);
    setup.harness.complete_terminations();
    assert!(setup.harness.supervisor.runners().is_empty());
    // The signed revocation waits, with no secret in it.
    let pending = setup.harness.supervisor.pending_revocation().expect("kept for a retry");
    assert_eq!(pending.machine_id, machine_id);
    let stored = serde_json::to_string(setup.harness.store.load().state()).unwrap();
    assert!(stored.contains("pendingRevocation") && !stored.contains("nestor_machine_"), "{stored}");
    setup.tick(1_000);
    assert!(setup.harness.supervisor.pending_revocation().is_some());
    assert!(!setup.nestor.is_revoked(&machine_id));

    setup.nestor.set_unreachable(false);
    setup.tick(1_000);
    assert!(setup.nestor.is_revoked(&machine_id), "revoked once Nestor is back");
    assert!(setup.harness.supervisor.pending_revocation().is_none());
    assert_eq!(setup.nestor.rejected_proofs(), 0, "the revocation was signed by the machine key");
}

#[test]
fn a_revocation_from_nestor_stops_everything() {
    let setup = setup();
    let machine_id = setup.associate();
    setup.nestor.set_queue(queue(1));
    setup.tick(1_000);
    setup.harness.bring_up();
    setup.harness.claim_all();
    setup.nestor.revoke_machine(&machine_id);
    setup.tick(1_000);
    assert_eq!(setup.harness.supervisor.connection(), Connection::Revoked);
    assert_eq!(setup.harness.supervisor.status().admission, Admission::Blocked(BlockReason::Revoked));
    assert!(setup.secrets.names().is_empty());
    assert!(setup.keys.load().unwrap().is_none());
    assert_eq!(setup.harness.launcher.terminated().len(), 1);
    // Associating again is possible afterwards.
    setup.harness.complete_terminations();
    setup.associate();
}

#[test]
fn startup_erases_leftover_secrets_and_drops_an_association_without_them() {
    let setup = setup();
    setup.secrets.put(MACHINE_CREDENTIAL, &Secret::new("nestor_machine_leftover_0000")).unwrap();
    setup.associator.startup();
    assert!(setup.secrets.names().is_empty(), "secrets without an association are erased");

    let setup = self::setup();
    setup.associate();
    setup.secrets.delete(MACHINE_CREDENTIAL).unwrap();
    setup.associator.startup();
    assert_eq!(setup.harness.supervisor.connection(), Connection::NotAssociated);
}

#[test]
fn a_live_service_without_the_nestor_api_does_not_pretend() {
    let harness = Harness::new();
    harness.supervisor.reconcile();
    let clock: Clock = Arc::new(|| 0);
    let associator = Associator::new(
        None,
        Arc::new(MemoryKeyStore::new()),
        Arc::new(MemorySecretStore::new()),
        harness.supervisor.clone(),
        clock,
    );
    assert_eq!(associator.associate(&machine()).unwrap_err().code(), "unavailable");
    assert_eq!(harness.supervisor.connection(), Connection::NotAssociated);
}

#[test]
fn demo_stores_never_touch_the_keychain() {
    let (secrets, keys) = stores_for(ServiceMode::Demo, None);
    assert_eq!(secrets.backend(), SecretBackend::Memory);
    assert_eq!(keys.protection(), KeyProtection::Memory);
    let (secrets, keys) = stores_for(ServiceMode::Live, None);
    assert_eq!(secrets.backend(), SecretBackend::Keychain);
    assert_eq!(keys.protection(), KeyProtection::SecureEnclave);
}

/// Unsigned test binaries cannot create a Secure Enclave key; the live key store must then fail
/// clearly instead of falling back to a software key (A6).
#[test]
fn without_signing_the_live_key_store_fails_instead_of_falling_back() {
    let enclave: Arc<dyn MachineKeyStore> = Arc::new(SecureEnclaveKeyStore {
        tag: format!("fr.kocoa.nestor.runner-manager.test.{}", std::process::id()),
        access_group: None,
    });
    let fallback_secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let fallback: Arc<dyn MachineKeyStore> = Arc::new(SoftwareKeyStore { secrets: fallback_secrets.clone() });
    let store = KeyStoreWithFallback::new(enclave.clone(), Some(fallback), false);
    match store.create() {
        Err(error) => {
            assert!(error.contains("signed build") && error.contains("fallback is disabled"), "{error}");
            assert!(fallback_secrets.get("machine-key-software").unwrap().is_none());
        }
        // A signed environment with the entitlement: the Secure Enclave key is real; clean up.
        Ok(_) => enclave.delete().unwrap(),
    }
}
