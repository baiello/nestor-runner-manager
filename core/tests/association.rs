//! Association against a fake Nestor (S9, B3, B6, B7, S8.5, N8, A6, M1 and M2 of KCA-738),
//! heartbeat, queue and leases (S4 to S6), as the machine routes of nestor #156 and #157 say.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use common::{queue, Harness};
use nestor_runner_core::association::{Associator, NESTOR_RUNNERS};
use nestor_runner_core::model::*;
use nestor_runner_core::nestor::{FakeNestor, MachineRequest, MachineResponse, NestorApi};
use nestor_runner_core::persist::{RevocationRequest, RunnerRecord};
use nestor_runner_core::process::CredentialIssuer;
use nestor_runner_core::secrets::*;
use nestor_runner_core::supervisor::Clock;

/// Between the service and the fake: answers the first `stale` requests `stale_machine_proof`,
/// and may claim another instance than the fake's.
struct Wire {
    nestor: Arc<FakeNestor>,
    instance: String,
    stale: AtomicU32,
    sent: std::sync::Mutex<Vec<String>>,
}

impl NestorApi for Wire {
    fn instance(&self) -> String {
        self.instance.clone()
    }

    fn send(&self, request: &MachineRequest) -> Result<MachineResponse, String> {
        self.sent.lock().unwrap().push(request.proof.clone());
        if self.stale.load(Ordering::SeqCst) > 0 {
            self.stale.fetch_sub(1, Ordering::SeqCst);
            return Ok(MachineResponse::error(401, "stale_machine_proof"));
        }
        self.nestor.send(request)
    }
}

struct Setup {
    harness: Harness,
    nestor: Arc<FakeNestor>,
    wire: Arc<Wire>,
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
    setup_for("nestor.test")
}

/// The service believes it talks to `instance`; the fake is `nestor.test`.
fn setup_for(instance: &str) -> Setup {
    let harness = Harness::new();
    harness.supervisor.reconcile();
    let nestor = Arc::new(FakeNestor::new("nestor.test"));
    nestor.set_now(harness.time.load(Ordering::SeqCst));
    // A server with the scope filter of S6 (nestor #157).
    nestor.set_scope_filter(true);
    let wire = Arc::new(Wire {
        nestor: nestor.clone(),
        instance: instance.into(),
        stale: AtomicU32::new(0),
        sent: Default::default(),
    });
    let secrets = Arc::new(MemorySecretStore::new());
    let keys = Arc::new(MemoryKeyStore::new());
    let time = harness.time.clone();
    let clock: Clock = Arc::new(move || time.load(Ordering::SeqCst));
    let associator = Arc::new(Associator::new(
        Some(wire.clone() as Arc<dyn NestorApi>),
        keys.clone(),
        secrets.clone(),
        harness.supervisor.clone(),
        clock,
    ));
    Setup { harness, nestor, wire, secrets, keys, associator }
}

fn record(id: &str) -> RunnerRecord {
    RunnerRecord {
        id: id.into(),
        name: "runner-1".into(),
        generation: 1,
        pid: None,
        start_time_us: None,
        launching_since: None,
        scope: None,
        created_at_ms: 0,
        manual: false,
    }
}

impl Setup {
    /// `ms` of service life: the association polls, sends its heartbeat when due and reads the
    /// queue, the supervisor regulates.
    fn tick(&self, ms: u64) {
        self.harness.advance(ms);
        self.nestor.set_now(self.harness.time.load(Ordering::SeqCst));
        let queue = self.associator.tick();
        self.harness.supervisor.tick(&queue);
    }

    /// Approved on Nestor, not confirmed on the machine yet.
    fn approve(&self) -> String {
        let connection = self.associator.associate(&machine()).unwrap();
        let Connection::Associating { user_code, .. } = connection else { panic!("{connection:?}") };
        assert!(self.nestor.approve(&user_code));
        self.tick(5_000);
        assert!(matches!(self.harness.supervisor.connection(), Connection::Confirming { .. }));
        self.harness.supervisor.machine_id().unwrap()
    }

    /// Approved and confirmed.
    fn associate(&self) -> String {
        let machine_id = self.approve();
        self.associator.confirm(true).unwrap();
        assert!(matches!(self.harness.supervisor.connection(), Connection::Associated { .. }));
        machine_id
    }

    fn requests(&self, target: &str) -> usize {
        self.nestor.requests().iter().filter(|r| r.ends_with(target)).count()
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

    fn errors(&self) -> String {
        let events = self.harness.supervisor.events_since(0).unwrap();
        events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ServiceError { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
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
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Confirming { .. }));
    assert_eq!(setup.nestor.public_key_of(&machine_id).unwrap(), key.public_key().unwrap(), "bound to the key");
    assert_eq!(setup.nestor.key_storage_of(&machine_id).unwrap(), "software", "a memory key is not declared enclave");
    assert!(setup.secrets.get(MACHINE_CREDENTIAL).unwrap().is_some());
    assert_eq!(setup.nestor.rejected_proofs(), 0, "every call carried a valid proof of possession");
    // The device code and the credential appear nowhere.
    let credential = setup.secrets.get(MACHINE_CREDENTIAL).unwrap().unwrap();
    let text = setup.everything_visible();
    assert!(!text.contains("nestor_device_") && !text.contains(credential.expose()), "{text}");
    // The user code is single use: approving it again does nothing.
    assert!(!setup.nestor.approve(user_code));
}

/// M1: Nestor's answer names the account, masked; nothing is created, no heartbeat, no queue,
/// no runner token, until the owner confirms it on the machine. A refusal disassociates.
#[test]
fn no_runner_before_the_owner_confirms_the_account() {
    let setup = setup();
    setup.nestor.set_queue(queue(3));
    let machine_id = setup.approve();
    let Connection::Confirming { account_email, approved_by_email, instance } = setup.harness.supervisor.connection()
    else {
        panic!()
    };
    assert_eq!((account_email.as_str(), approved_by_email.as_str()), ("o***r@example.com", "a***n@example.com"));
    assert_eq!(instance, "nestor.test");
    assert_eq!(setup.harness.supervisor.status().admission, Admission::Blocked(BlockReason::AwaitingConfirmation));
    for _ in 0..3 {
        setup.tick(15_000);
    }
    assert_eq!(setup.harness.launcher.spawns(), 0);
    assert_eq!(setup.requests("/machine/heartbeat") + setup.requests("/machine/queue"), 0);
    assert!(setup.associator.issue(&record("0123456789abcdef")).is_err(), "no token before the confirmation");
    assert!(setup.nestor.runners_of(&machine_id).is_empty());
    // Kept across a restart of the service, still waiting.
    setup.associator.startup();
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Confirming { .. }));
    assert!(setup.associator.issue(&record("0123456789abcdef")).is_err());

    setup.associator.confirm(true).unwrap();
    setup.tick(1_000);
    assert_eq!(setup.harness.launcher.spawns(), 1, "launched once confirmed");

    // Refused: the machine disassociates itself and erases its secrets.
    let setup = self::setup();
    let machine_id = setup.approve();
    assert_eq!(setup.associator.confirm(false).unwrap(), Connection::NotAssociated);
    assert!(setup.nestor.is_revoked(&machine_id), "Nestor was told");
    assert!(setup.secrets.names().is_empty() && setup.keys.load().unwrap().is_none());
    assert!(setup.associator.confirm(true).is_err(), "nothing left to confirm");
}

/// M2: the queue, then a runner's token, in the same millisecond: each proof signed after the
/// last, none refused.
#[test]
fn every_request_is_signed_after_the_last_even_in_the_same_millisecond() {
    let setup = setup();
    setup.associate();
    setup.nestor.set_queue(queue(1));
    // The clock does not move between these calls.
    let queue = setup.associator.tick();
    assert_eq!(queue.total(), 1);
    setup.associator.issue(&record("0123456789abcdef")).unwrap();
    setup.associator.issue(&record("0123456789abcdef")).unwrap();
    setup.associator.tick();
    assert_eq!(setup.nestor.stale_proofs(), 0, "{:?}", setup.nestor.accepted_proofs());
    assert_eq!(setup.nestor.rejected_proofs(), 0);
    let times = setup.nestor.accepted_proofs();
    assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    assert!(times.len() >= 6, "{times:?}");
}

#[test]
fn a_stale_proof_is_signed_again_with_a_new_time_and_sent_again() {
    let setup = setup();
    setup.associate();
    setup.wire.sent.lock().unwrap().clear();
    setup.wire.stale.store(2, Ordering::SeqCst);
    setup.nestor.set_queue(queue(2));
    setup.tick(1_000);
    let sent = setup.wire.sent.lock().unwrap().clone();
    let times: Vec<u64> = sent.iter().map(|p| p.split('.').next().unwrap().parse().unwrap()).collect();
    assert!(times.len() >= 3 && times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    assert_eq!(setup.harness.launcher.spawns(), 1, "the request went through");

    // Refused again and again: the clock is off, and the owner is told.
    setup.wire.stale.store(100, Ordering::SeqCst);
    setup.tick(15_000);
    assert!(setup.errors().contains("clock"), "{}", setup.errors());
}

/// The audience is the API the service was configured with: proofs meant for another instance
/// pass on none.
#[test]
fn a_proof_for_another_instance_is_refused() {
    let setup = setup_for("other.test");
    assert!(setup.associator.associate(&machine()).is_err());
    assert_eq!(setup.nestor.rejected_proofs(), 1);
}

#[test]
fn the_heartbeat_reports_what_the_service_applies_and_the_server_terms_follow() {
    let setup = setup();
    let machine_id = setup.associate();
    setup.harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    setup.tick(1_000);
    let heartbeats = setup.nestor.heartbeats(&machine_id);
    let first = heartbeats.last().expect("a heartbeat once confirmed");
    assert_eq!(first["machine_protocol_version"], 1);
    assert_eq!(first["service_mode"], "demo");
    assert_eq!(first["mode"], "automatic");
    assert!(first.get("cpu_percent").is_none_or(|v| v.as_f64().is_some_and(|c| c.fract() == 0.0)));
    // Administrator limit 1 by default: the ceiling applied is 1, whatever is asked.
    let status = setup.harness.supervisor.status();
    assert_eq!((status.max_concurrent_tasks, status.effective_max_concurrent_tasks), (4, 1));
    assert!(status.capped_by_server);

    // Raised to 3 on the web: 3, as the scope filter is announced.
    setup.nestor.set_administrator_limits(&machine_id, 3, false);
    setup.tick(1_000);
    assert_eq!(setup.harness.supervisor.status().effective_max_concurrent_tasks, 3);
    // Not every second: at the server's interval, 15 s.
    let sent = setup.nestor.heartbeats(&machine_id).len();
    setup.tick(1_000);
    assert_eq!(setup.nestor.heartbeats(&machine_id).len(), sent);
    setup.tick(15_000);
    assert_eq!(setup.nestor.heartbeats(&machine_id).len(), sent + 1);
    assert_eq!(setup.nestor.heartbeats(&machine_id).last().unwrap()["max_concurrent_tasks"], 3);

    // A server without the scope filter: one at a time again.
    setup.nestor.set_scope_filter(false);
    setup.tick(1_000);
    assert_eq!(setup.harness.supervisor.status().effective_max_concurrent_tasks, 1);

    // Paused on the web: nothing new starts, nothing running is interrupted.
    setup.nestor.set_scope_filter(true);
    setup.nestor.set_queue(queue(5));
    setup.tick(1_000);
    setup.harness.bring_up();
    let claimed = setup.harness.claim_all();
    assert_eq!(claimed.len(), 3);
    setup.nestor.set_administrator_limits(&machine_id, 3, true);
    setup.tick(1_000);
    assert_eq!(setup.harness.supervisor.status().admission, Admission::Blocked(BlockReason::PausedOnNestor));
    for id in &claimed {
        setup.harness.finish(id);
    }
    setup.tick(1_000);
    assert_eq!(setup.harness.launcher.spawns(), 3, "no launch while paused on Nestor");
    assert!(setup.harness.launcher.terminated().is_empty());
}

/// With the scope filter announced and more than one task at a time, every live runner holds
/// the same scope; another scope waits until they end (B1(b)). No `--scope` is passed.
#[test]
fn several_tasks_at_a_time_never_mix_scopes() {
    let setup = setup();
    let machine_id = setup.associate();
    setup.nestor.set_administrator_limits(&machine_id, 4, false);
    setup.harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    let mut eligible = std::collections::BTreeMap::new();
    eligible.insert("scope-a".to_string(), 2);
    eligible.insert("scope-b".to_string(), 3);
    setup.nestor.set_queue(Queue { eligible });
    setup.tick(1_000);
    setup.tick(1_000);
    setup.harness.bring_up();
    let live: Vec<_> = setup.harness.launcher.live().into_iter().map(|p| p.scope).collect();
    assert_eq!(live.len(), 3, "{live:?}");
    assert!(live.iter().all(|s| s.as_deref() == Some("scope-b")), "{live:?}");
    assert_eq!(setup.harness.supervisor.status().active_scope.as_deref(), Some("scope-b"));
    // scope-a waits until every scope-b runner ended.
    let claimed = setup.harness.claim_all();
    assert_eq!(claimed.len(), 3);
    setup.nestor.set_queue(Queue::single("scope-a", 2));
    for id in &claimed[..2] {
        setup.harness.finish(id);
    }
    setup.tick(1_000);
    let scopes: std::collections::BTreeSet<_> = setup.harness.launcher.live().into_iter().map(|p| p.scope).collect();
    assert_eq!(scopes.len(), 1, "{scopes:?}");
    setup.harness.finish(&claimed[2]);
    setup.tick(1_000);
    setup.harness.bring_up();
    let live: Vec<_> = setup.harness.launcher.live().into_iter().map(|p| p.scope).collect();
    assert!(!live.is_empty() && live.iter().all(|s| s.as_deref() == Some("scope-a")), "{live:?}");
}

#[test]
fn an_unreachable_nestor_blocks_launches_and_recovers() {
    let setup = setup();
    setup.associate();
    setup.nestor.set_queue(queue(2));
    setup.tick(1_000);
    assert_eq!(setup.harness.launcher.spawns(), 1);
    setup.nestor.set_unreachable(true);
    setup.tick(15_000);
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Unreachable { .. }));
    setup.nestor.set_unreachable(false);
    setup.tick(15_000);
    assert!(matches!(setup.harness.supervisor.connection(), Connection::Associated { .. }));
}

/// A runner of the machine on Nestor for each local runner: created once, then given a new
/// token for each launch; created again if Nestor revoked it.
#[test]
fn runner_tokens_are_issued_per_launch_with_a_proof() {
    let setup = setup();
    let machine_id = setup.associate();
    let first = setup.associator.issue(&record("0123456789abcdef0123456789abcdef")).unwrap();
    let second = setup.associator.issue(&record("0123456789abcdef0123456789abcdef")).unwrap();
    assert_ne!(first, second, "a new token for each launch");
    assert_eq!(setup.nestor.runner_credentials(&machine_id).len(), 2);
    assert_eq!(setup.nestor.runners_of(&machine_id).len(), 1, "one runner of the machine for it");
    let tokens =
        setup.nestor.requests().iter().filter(|r| r.contains("/machine/runners/") && r.ends_with("/token")).count();
    assert_eq!((setup.requests("/machine/runners"), tokens), (1, 1));
    assert!(setup.nestor.accepts_runner_token(second.expose()));
    assert!(!setup.nestor.accepts_runner_token(first.expose()), "the previous token died");
    let kept = setup.secrets.get(NESTOR_RUNNERS).unwrap().unwrap();
    assert!(!kept.expose().contains("nestor_runner_"), "no token kept: {}", kept.expose());

    setup.nestor.revoke_runner(&machine_id, &setup.nestor.runners_of(&machine_id)[0]);
    setup.associator.issue(&record("0123456789abcdef0123456789abcdef")).unwrap();
    assert_eq!(setup.nestor.runners_of(&machine_id).len(), 2, "a new one stands for it");
    setup.associator.issue(&record("fedcba9876543210fedcba9876543210")).unwrap();
    assert_eq!(setup.nestor.runners_of(&machine_id).len(), 3);
    assert_eq!(setup.nestor.rejected_proofs() + setup.nestor.stale_proofs(), 0);
}

#[test]
fn disassociating_tells_nestor_and_offline_erases_secrets_at_once() {
    let setup = setup();
    let machine_id = setup.associate();
    setup.associator.disassociate();
    assert!(setup.nestor.is_revoked(&machine_id), "POST /machine/disassociate");
    assert_eq!(setup.nestor.revocations(), vec![machine_id]);

    let setup = self::setup();
    let machine_id = setup.associate();
    setup.nestor.set_administrator_limits(&machine_id, 2, false);
    setup.harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    setup.nestor.set_queue(queue(2));
    setup.tick(1_000);
    setup.tick(1_000);
    setup.harness.bring_up();
    assert_eq!(setup.harness.claim_all().len(), 2);

    setup.nestor.set_unreachable(true);
    let connection = setup.associator.disassociate();
    assert_eq!(connection, Connection::NotAssociated);
    // Secrets erased even though Nestor cannot be reached.
    assert!(setup.secrets.names().is_empty());
    assert!(setup.keys.load().unwrap().is_none());
    // Runners terminated: disassociation is an exception to "never interrupt".
    assert_eq!(setup.harness.launcher.terminated().len(), 2);
    setup.harness.complete_terminations();
    assert!(setup.harness.supervisor.runners().is_empty());
    // No revocation signed for later (a proof is good 60 s): the owner is told to revoke on Nestor.
    assert!(setup.harness.supervisor.pending_revocation().is_none());
    assert!(setup.errors().contains("revoke this machine on the Nestor web app"), "{}", setup.errors());
    assert!(!setup.nestor.is_revoked(&machine_id));
    let stored = serde_json::to_string(setup.harness.store.load().state()).unwrap();
    assert!(!stored.contains("nestor_machine_"), "{stored}");
}

#[test]
fn a_revocation_signed_by_an_earlier_release_is_dropped_with_a_word() {
    let setup = setup();
    setup.harness.supervisor.set_pending_revocation(Some(RevocationRequest {
        instance: "nestor.test".into(),
        machine_id: "00000000-0000-4000-8000-000000000001".into(),
        issued_at_ms: 1,
        signature: "00".into(),
    }));
    setup.associator.startup();
    assert!(setup.harness.supervisor.pending_revocation().is_none());
    assert!(setup.errors().contains("revoke it on the Nestor web app"), "{}", setup.errors());
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
    setup.tick(15_000);
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
